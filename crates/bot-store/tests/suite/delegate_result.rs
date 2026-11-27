use crate::support::*;
use bot_core::{command::Delegate, id::*, port::*};
use serde_json::{Value, json};

#[tokio::test]
async fn child_result_requires_its_original_grants_target_and_current_revision() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut request = accept("personal", "child-result", "Ask the child");
    request.scopes = scopes(&["run", "report.read"]);
    db.store.accept(request).await.unwrap();
    let parent = db.store.claim().await.unwrap().unwrap();
    let mut command = Delegate {
        parent: parent.attempt,
        checked_revision: Revision("r1".into()),
        step: "report".into(),
        child: AgentId("main/research".into()),
        instruction: "Read the report".into(),
        scopes: scopes(&["run", "report.read"]),
    };
    assert!(db.store.result(command.clone()).await.unwrap().is_none());
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    db.store.delegate(command.clone()).await.unwrap();
    let child = db.store.claim().await.unwrap().unwrap();
    db.store
        .finish(child.attempt, done("visible"))
        .await
        .unwrap();
    assert!(matches!(
        db.store.result(command.clone()).await,
        Err(Error::Stale)
    ));
    command.parent = db.store.claim().await.unwrap().unwrap().attempt;
    let result = db.store.result(command.clone()).await.unwrap().unwrap();
    let body: Value = serde_json::from_str(&result.0).unwrap();
    assert_eq!(
        body,
        json!({"state":"succeeded","result":{"text":"visible"}})
    );
    let mut revoked = command.clone();
    revoked.scopes.remove("report.read");
    assert!(matches!(db.store.result(revoked).await, Err(Error::Denied)));
    let mut changed = command.clone();
    changed.instruction.push('!');
    assert!(matches!(
        db.store.result(changed).await,
        Err(Error::Conflict)
    ));
    let mut other = command.clone();
    other.child = AgentId("main/other".into());
    assert!(matches!(db.store.result(other).await, Err(Error::Conflict)));
    command.checked_revision = Revision("old".into());
    assert!(matches!(db.store.result(command).await, Err(Error::Stale)));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 2);
}

#[tokio::test]
async fn publishing_a_new_policy_before_delegation_commits_keeps_the_parent_leased() {
    use sqlx::Connection;
    use std::time::Duration;
    let Some(db) = TestDb::new().await else {
        return;
    };
    db.store
        .accept(accept("personal", "race", "Delegate"))
        .await
        .unwrap();
    let parent = db.store.claim().await.unwrap().unwrap();
    let command = Delegate {
        parent: parent.attempt,
        checked_revision: Revision("r1".into()),
        step: "report".into(),
        child: AgentId("main/research".into()),
        instruction: "Read the report".into(),
        scopes: scopes(&["run"]),
    };
    let peer = db.peer("stale-delegation").await;
    let mut connection = db.raw().await;
    let mut publication = connection.begin().await.unwrap();
    let writer: i32 = sqlx::query_scalar(
        "update bot_active set revision = 'set-2' where singleton returning pg_backend_pid()",
    )
    .fetch_one(&mut *publication)
    .await
    .unwrap();
    sqlx::query("update bot_active_profiles set revision = 'r2' where profile = 'personal'")
        .execute(&mut *publication)
        .await
        .unwrap();
    let task = tokio::spawn(async move { peer.delegate(command).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "select exists(select 1 from pg_stat_activity where $1 = any(pg_blocking_pids(pid)))",
            ).bind(writer).fetch_one(db.store.pool()).await.unwrap();
            if waiting { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("delegate must wait for publication");
    publication.commit().await.unwrap();
    assert!(matches!(task.await.unwrap(), Err(Error::Stale)));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    assert_eq!(
        db.scalar::<String>("select state from bot_runs").await,
        "leased"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action='run.delegate'")
            .await,
        0
    );
}
