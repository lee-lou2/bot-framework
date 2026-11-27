//! Memory scope and read fencing: edits/deletes obey ancestor-only visibility, and reads re-verify the active claim atomically before committing private marks.

use crate::support::*;
use bot_core::{command::*, id::*, port::*, state::ConversationKind};
use std::time::Duration;

async fn agent_run(db: &TestDb, request: &str, agent: &str) -> RunContext {
    db.publish().await;
    let mut a = accept_as(
        "personal",
        request,
        OWNER,
        ConversationKind::Cli,
        "c1",
        "hi",
    );
    a.target = Target::Agent {
        agent: AgentId(agent.into()),
        instruction: "hi".into(),
    };
    runs(db).accept(a).await.unwrap();
    runs(db).claim().await.unwrap().unwrap()
}

fn edit(id: &MemoryId, text: &str) -> MemoryChange {
    MemoryChange::Edit {
        id: id.clone(),
        text: text.into(),
    }
}

use bot_core::state::RunContext;

#[tokio::test]
async fn edit_and_delete_follow_the_same_agent_scope_as_reading() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let parent = agent_run(&db, "p", "main").await;
    let research = agent_run(&db, "r", "main/research").await;
    let other = agent_run(&db, "o", "main/other").await;
    let from_parent = create(&db, &parent, "parent note").await;
    let from_research = create(&db, &research, "research note").await;

    // Sibling and parent agents cannot mutate a descendant's memory even under the same owner.
    for run in [&other, &parent] {
        assert!(matches!(
            memories(&db)
                .change(run.attempt.clone(), edit(&from_research, "tampered"))
                .await,
            Err(Error::Denied)
        ));
        assert!(matches!(
            memories(&db)
                .change(
                    run.attempt.clone(),
                    MemoryChange::Delete {
                        id: from_research.clone()
                    }
                )
                .await,
            Err(Error::Denied)
        ));
    }
    assert_eq!(
        db.scalar::<String>(&format!(
            "select body from bot_memories where id = '{}'",
            from_research.0
        ))
        .await,
        "research note"
    );
    memories(&db)
        .change(research.attempt.clone(), edit(&from_parent, "edited"))
        .await
        .unwrap();
    memories(&db)
        .change(
            research.attempt.clone(),
            edit(&from_research, "self-edited"),
        )
        .await
        .unwrap();
    memories(&db)
        .change(
            research.attempt.clone(),
            MemoryChange::Delete { id: from_research },
        )
        .await
        .unwrap();
}

/// Holds a `cancel_requested` update open on `bot_runs` until the concurrent read blocks on it, then commits.
async fn racing_cancel<F, Fut>(db: &TestDb, run: &RunContext, read: F) -> Result<(), Error>
where
    F: FnOnce(TestDbRef) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), Error>> + Send + 'static,
{
    let mut connection = db.raw().await;
    let mut cancelling = sqlx::Connection::begin(&mut connection).await.unwrap();
    let writer: i32 = sqlx::query_scalar("select pg_backend_pid()")
        .fetch_one(&mut *cancelling)
        .await
        .unwrap();
    sqlx::query("update bot_runs set cancel_requested = true where id = $1")
        .bind(&run.attempt.run.0)
        .execute(&mut *cancelling)
        .await
        .unwrap();
    let peer = db.peer("reader").await;
    let reading = tokio::spawn(read(TestDbRef(peer)));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = sqlx::query_scalar(
                "select exists(select 1 from pg_stat_activity where $1 = any(pg_blocking_pids(pid)))",
            )
            .bind(writer)
            .fetch_one(db.store.pool())
            .await
            .unwrap();
            if waiting {
                break;
            }
            assert!(!reading.is_finished(), "read finished before cancel commit");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("read must wait on the run row lock");
    cancelling.commit().await.unwrap();
    reading.await.unwrap()
}

struct TestDbRef(bot_store::Store);

#[tokio::test]
async fn a_cancel_that_lands_before_the_result_is_confirmed_stops_the_read_and_the_private_mark() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = agent_run(&db, "s", "main").await;
    let id = create(&db, &run, "private note").await;
    let attempt = run.attempt.clone();
    let result = racing_cancel(&db, &run, move |peer| async move {
        MemoryStore::search(&peer.0, attempt, "note".into())
            .await
            .map(|_| ())
    })
    .await;
    assert!(matches!(result, Err(Error::Stale)));
    assert!(
        !flag(&db, &run).await,
        "aborted read must not set private_memory_read"
    );

    db.sql("update bot_runs set cancel_requested = false").await;
    let attempt = run.attempt.clone();
    let result = racing_cancel(&db, &run, move |peer| async move {
        MemoryStore::preview(&peer.0, attempt, id).await.map(|_| ())
    })
    .await;
    assert!(matches!(result, Err(Error::Stale)));
    assert!(!flag(&db, &run).await);
}

#[tokio::test]
async fn a_read_that_is_confirmed_commits_the_private_mark_with_the_result() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = agent_run(&db, "s", "main").await;
    let id = create(&db, &run, "private note").await;
    assert!(!flag(&db, &run).await);
    assert_eq!(texts(&db, &run, "note").await, ["private note"]);
    assert!(flag(&db, &run).await);
    db.sql("update bot_runs set private_memory_read = false")
        .await;
    memories(&db)
        .preview(run.attempt.clone(), id)
        .await
        .unwrap();
    assert!(flag(&db, &run).await);
}
