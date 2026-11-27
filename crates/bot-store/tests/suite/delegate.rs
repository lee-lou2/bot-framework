#[path = "delegate/cancellation.rs"]
mod cancellation;

use crate::support::*;
use bot_core::{command::*, id::*, port::*, state::*};

fn profile() -> ProfileId {
    ProfileId("personal".into())
}

fn delegate(
    parent: &Attempt,
    step: &str,
    child: &str,
    instruction: &str,
    granted: &[&str],
) -> Delegate {
    Delegate {
        checked_revision: bot_core::id::Revision("r1".into()),
        parent: parent.clone(),
        step: step.into(),
        child: AgentId(child.into()),
        instruction: instruction.into(),
        scopes: scopes(granted),
    }
}

async fn state_of(db: &TestDb, run: &RunId) -> String {
    db.scalar::<String>(&format!(
        "select state from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

async fn delegated(db: &TestDb) -> (RunContext, RunId) {
    let parent_run = db
        .store
        .accept(accept("personal", "r1", "please investigate"))
        .await
        .unwrap();
    let parent = db.store.claim().await.unwrap().unwrap();
    assert_eq!(parent.attempt.run, parent_run);
    let child = db
        .store
        .delegate(delegate(
            &parent.attempt,
            "1",
            "main/research",
            "check schedule context",
            &["run"],
        ))
        .await
        .unwrap();
    (parent, child)
}

#[tokio::test]
async fn delegation_parks_the_parent_and_wakes_it_once_with_the_child_result() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (parent, child) = delegated(&db).await;
    assert_eq!(state_of(&db, &parent.attempt.run).await, "waiting");
    assert_eq!(
        db.scalar::<String>(
            "select wait_kind || ':' || wait_child from bot_runs where parent is null"
        )
        .await,
        format!("child:{}", child.0)
    );
    assert!(
        matches!(
            db.store.renew(parent.attempt.clone()).await,
            Err(Error::Stale)
        ),
        "waiting parent's lease was released"
    );

    let got = db.store.claim().await.unwrap().unwrap();
    assert_eq!(got.attempt.run, child);
    assert_eq!(
        (
            got.root.clone(),
            got.principal.0.as_str(),
            got.agent.0.as_str()
        ),
        (parent.root.clone(), "local:owner", "main/research")
    );
    assert_eq!(
        (got.conversation.kind.clone(), got.conversation.id.as_str()),
        (ConversationKind::Cli, "owner")
    );
    assert_eq!(got.scopes, scopes(&["run"]));
    assert!(
        db.store.claim().await.unwrap().is_none(),
        "parent is not claimable before child finishes"
    );

    db.store
        .finish(got.attempt.clone(), done("research result"))
        .await
        .unwrap();
    assert_eq!(state_of(&db, &parent.attempt.run).await, "ready");
    let resumed = db.store.claim().await.unwrap().unwrap();
    assert_resumed(&parent.attempt, &resumed.attempt);
    let step = &resumed.work.steps[0];
    assert_eq!(step.key, "1");
    assert_eq!(
        step.body.0,
        r#"{"result":{"text":"research result"},"state":"succeeded"}"#
    );

    assert!(matches!(
        db.store.finish(got.attempt, done("again")).await,
        Err(Error::Stale)
    ));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_steps").await, 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.wake'")
            .await,
        1
    );
    assert_eq!(db.store.verify().await.unwrap().first_bad, None);
}

#[tokio::test]
async fn resumed_parent_gets_the_same_child_for_the_same_step() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (parent, child) = delegated(&db).await;
    let got = db.store.claim().await.unwrap().unwrap();
    db.store.finish(got.attempt, done("x")).await.unwrap();
    let resumed = db.store.claim().await.unwrap().unwrap();
    let again = db
        .store
        .delegate(delegate(
            &resumed.attempt,
            "1",
            "main/research",
            "check schedule context",
            &["run"],
        ))
        .await
        .unwrap();
    assert_eq!(again, child);
    assert_eq!(state_of(&db, &parent.attempt.run).await, "leased");
    let changed = db
        .store
        .delegate(delegate(
            &resumed.attempt,
            "1",
            "main/research",
            "different instruction",
            &["run"],
        ))
        .await;
    assert!(matches!(changed, Err(Error::Conflict)));
    let wider = db
        .store
        .delegate(delegate(
            &resumed.attempt,
            "1",
            "main/research",
            "check schedule context",
            &["run", "calendar.read"],
        ))
        .await;
    assert!(matches!(wider, Err(Error::Conflict) | Err(Error::Denied)));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 2);
}

#[tokio::test]
async fn failed_and_cancelled_children_wake_the_parent_with_their_outcome() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (parent, _child) = delegated(&db).await;
    let got = db.store.claim().await.unwrap().unwrap();
    assert_eq!(
        db.store
            .fail(got.attempt, Failure::Permanent("invalid_output"))
            .await
            .unwrap(),
        Retry::Dead
    );
    let resumed = db.store.claim().await.unwrap().unwrap();
    assert_eq!(resumed.attempt.run, parent.attempt.run);
    assert_eq!(
        resumed.work.steps[0].body.0,
        r#"{"error":"invalid_output","state":"dead"}"#
    );

    let second = db
        .store
        .delegate(delegate(
            &resumed.attempt,
            "2",
            "main/research",
            "again",
            &["run"],
        ))
        .await
        .unwrap();
    db.store
        .cancel(
            profile(),
            second,
            PrincipalId("local:owner".into()),
            "stop".into(),
        )
        .await
        .unwrap();
    let again = db.store.claim().await.unwrap().unwrap();
    assert_eq!(
        again.work.steps[1].body.0,
        r#"{"error":"cancelled","state":"cancelled"}"#
    );
}

#[tokio::test]
async fn resuming_a_dead_child_never_wakes_the_parent_twice() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (parent, child) = delegated(&db).await;
    let got = db.store.claim().await.unwrap().unwrap();
    db.store
        .fail(got.attempt, Failure::Permanent("invalid_output"))
        .await
        .unwrap();
    let resumed = db.store.claim().await.unwrap().unwrap();
    assert_eq!(resumed.attempt.run, parent.attempt.run);

    // Resuming a dead child and finishing it must not wake the parent a second time.
    db.store
        .resume(
            profile(),
            child.clone(),
            PrincipalId("local:owner".into()),
            "fixed".into(),
        )
        .await
        .unwrap();
    let retry = db.store.claim().await.unwrap().unwrap();
    assert_eq!(retry.attempt.run, child);
    db.store
        .finish(retry.attempt, done("succeeded now"))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_steps").await,
        1,
        "checkpoint recorded once"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.wake'")
            .await,
        1
    );
    assert_eq!(
        state_of(&db, &parent.attempt.run).await,
        "leased",
        "parent stays leased by its owner"
    );
}

#[tokio::test]
async fn delegation_rules_reject_unsafe_requests() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.store
        .accept(accept("personal", "r1", "hi"))
        .await
        .unwrap();
    let parent = db.store.claim().await.unwrap().unwrap();
    let denied = [
        ("main/research/deep", &["run"][..]), // not a direct child
        ("main", &["run"][..]),               // self
        ("other/research", &["run"][..]),     // different lineage
        ("main/research", &["run", "calendar.delete"][..]), // scope outside parent ceiling
    ];
    for (child, granted) in denied {
        let result = db
            .store
            .delegate(delegate(&parent.attempt, "1", child, "x", granted))
            .await;
        assert!(matches!(result, Err(Error::Denied)), "{child}");
    }
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    assert_eq!(
        state_of(&db, &parent.attempt.run).await,
        "leased",
        "denied delegation leaves parent unchanged"
    );
    let empty = db
        .store
        .delegate(delegate(
            &parent.attempt,
            "1",
            "main/research",
            "  ",
            &["run"],
        ))
        .await;
    assert!(matches!(empty, Err(Error::Invalid)));

    // A parent with cancel_requested cannot delegate new work.
    db.store
        .cancel(
            profile(),
            parent.attempt.run.clone(),
            PrincipalId("local:owner".into()),
            "x".into(),
        )
        .await
        .unwrap();
    let late = db
        .store
        .delegate(delegate(
            &parent.attempt,
            "2",
            "main/research",
            "x",
            &["run"],
        ))
        .await;
    assert!(matches!(late, Err(Error::Denied)));
}

#[tokio::test]
async fn cancelling_a_parent_cancels_the_waiting_tree_without_waking_it() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (parent, child) = delegated(&db).await;
    db.store
        .cancel(
            profile(),
            parent.attempt.run.clone(),
            PrincipalId("local:owner".into()),
            "stop".into(),
        )
        .await
        .unwrap();
    assert_eq!(state_of(&db, &parent.attempt.run).await, "cancelled");
    assert_eq!(state_of(&db, &child).await, "cancelled");
    assert!(db.store.claim().await.unwrap().is_none());
    assert_eq!(db.scalar::<i64>("select count(*) from bot_steps").await, 0);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.wake'")
            .await,
        0
    );
}

#[tokio::test]
async fn child_answers_are_not_delivered() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let dm = accept_as(
        "personal",
        "r1",
        "slack:home/U1",
        ConversationKind::Direct,
        "D1",
        "hi",
    );
    db.store.accept(dm).await.unwrap();
    let parent = db.store.claim().await.unwrap().unwrap();
    db.store
        .delegate(delegate(
            &parent.attempt,
            "1",
            "main/research",
            "x",
            &["run"],
        ))
        .await
        .unwrap();
    let got = db.store.claim().await.unwrap().unwrap();
    db.store
        .finish(got.attempt, done("child reply"))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_outbox").await,
        0,
        "only parent replies"
    );
    let resumed = db.store.claim().await.unwrap().unwrap();
    db.store
        .finish(resumed.attempt, done("parent reply"))
        .await
        .unwrap();
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 1);
}

#[tokio::test]
async fn cancel_covers_child_committed_while_waiting_for_parent() {
    for grandchild in [false, true] {
        let Some(db) = TestDb::new().await else {
            return;
        };
        let root = leased(&db, "tree").await;
        let parent = if grandchild {
            db.store
                .delegate(delegate(
                    &root.attempt,
                    "first",
                    "main/a",
                    "investigate",
                    &["run"],
                ))
                .await
                .unwrap();
            db.store.claim().await.unwrap().unwrap()
        } else {
            root.clone()
        };
        let mut lock = db.store.pool().begin().await.unwrap();
        let pid: i32 = sqlx::query_scalar(
            "select pg_backend_pid() from bot_audit_head where singleton for update",
        )
        .fetch_one(&mut *lock)
        .await
        .unwrap();
        let peer = db.peer("delegate").await;
        let cmd = delegate(
            &parent.attempt,
            "next",
            &format!("{}/b", parent.agent.0),
            "investigate",
            &["run"],
        );
        let creating = tokio::spawn(async move { peer.delegate(cmd).await });
        let creating_pid = db.waiter_blocked_by(pid).await;
        let admin = db.peer("admin").await;
        let target = root.attempt.run.clone();
        let cancelling = tokio::spawn(async move {
            admin
                .cancel(
                    profile(),
                    target,
                    PrincipalId("operator".into()),
                    "stop".into(),
                )
                .await
        });
        db.waiter_blocked_by(creating_pid).await;
        lock.commit().await.unwrap();
        let child = creating.await.unwrap().unwrap();
        cancelling.await.unwrap().unwrap();
        for id in [&root.attempt.run, &parent.attempt.run, &child] {
            assert_eq!(state_of(&db, id).await, "cancelled");
        }
        assert!(db.store.claim().await.unwrap().is_none());
    }
}

#[tokio::test]
async fn delegation_waiting_for_a_committing_cancel_cannot_create_a_child() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let root = leased(&db, "first-cancel").await;
    let mut lock = db.store.pool().begin().await.unwrap();
    let pid: i32 = sqlx::query_scalar(
        "select pg_backend_pid() from bot_audit_head where singleton for update",
    )
    .fetch_one(&mut *lock)
    .await
    .unwrap();
    let admin = db.peer("admin").await;
    let target = root.attempt.run.clone();
    let cancelling = tokio::spawn(async move {
        admin
            .cancel(
                profile(),
                target,
                PrincipalId("operator".into()),
                "stop".into(),
            )
            .await
    });
    let cancelling_pid = db.waiter_blocked_by(pid).await;
    let peer = db.peer("delegate").await;
    let cmd = delegate(&root.attempt, "next", "main/a", "investigate", &["run"]);
    let creating = tokio::spawn(async move { peer.delegate(cmd).await });
    db.waiter_blocked_by(cancelling_pid).await;
    lock.commit().await.unwrap();
    cancelling.await.unwrap().unwrap();
    assert!(matches!(creating.await.unwrap(), Err(Error::Denied)));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
}

#[tokio::test]
async fn cancelling_a_subtree_leaves_its_sibling_alone() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let (root, first) = delegated(&db).await;
    let child = db.store.claim().await.unwrap().unwrap();
    db.store
        .fail(child.attempt, Failure::Permanent("failed"))
        .await
        .unwrap();
    let parent = db.store.claim().await.unwrap().unwrap();
    let second = db
        .store
        .delegate(delegate(
            &parent.attempt,
            "2",
            "main/b",
            "investigate",
            &["run"],
        ))
        .await
        .unwrap();
    db.store
        .resume(
            profile(),
            first.clone(),
            PrincipalId("operator".into()),
            "resume".into(),
        )
        .await
        .unwrap();
    db.store
        .cancel(
            profile(),
            first.clone(),
            PrincipalId("operator".into()),
            "stop".into(),
        )
        .await
        .unwrap();
    assert_eq!(state_of(&db, &root.attempt.run).await, "waiting");
    assert_eq!(state_of(&db, &first).await, "cancelled");
    assert_eq!(db.store.claim().await.unwrap().unwrap().attempt.run, second);
}
