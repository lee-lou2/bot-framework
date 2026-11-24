//! 기억 경계(PG): 수정·삭제도 읽기와 같은 에이전트 범위(만든 에이전트의 자손에게만 보인다)이고,
//! 읽기 결과를 돌려주는 순간 소유자(claim·리스·취소 없음)를 같은 트랜잭션에서 다시 확인한다.

mod support;

use bot_core::{command::*, id::*, port::*, state::ConversationKind};
use std::time::Duration;
use support::*;

/// 지정 에이전트 대상으로 접수·claim한 owner의 CLI run.
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
    let from_parent = create(&db, &parent, "부모가 만든 메모").await;
    let from_research = create(&db, &research, "연구가 만든 메모").await;

    // 같은 소유자라도 형제·부모 에이전트는 자손이 만든 기억을 고치거나 지우지 못한다(보이지 않는 것과 같다)
    for run in [&other, &parent] {
        assert!(matches!(
            memories(&db)
                .change(run.attempt.clone(), edit(&from_research, "바꿈"))
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
        "연구가 만든 메모"
    );
    // 조상이 만든 기억은 자손 에이전트가 보므로 소유자라면 고칠 수 있다
    memories(&db)
        .change(research.attempt.clone(), edit(&from_parent, "고침"))
        .await
        .unwrap();
    // 만든 에이전트 자신은 고치고 지운다
    memories(&db)
        .change(research.attempt.clone(), edit(&from_research, "내가 고침"))
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

/// 다른 트랜잭션이 run 행을 쥐고 취소를 표시하는 동안 읽기가 들어온다: 취소가 먼저 커밋되면 읽기는 결과도 표시도 남기지 않는다.
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
            assert!(!reading.is_finished(), "읽기가 취소 표시보다 먼저 끝났다");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("읽기가 run 행의 변경을 기다려야 한다");
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
    let id = create(&db, &run, "비공개 메모").await;
    let attempt = run.attempt.clone();
    let result = racing_cancel(&db, &run, move |peer| async move {
        MemoryStore::search(&peer.0, attempt, "메모".into())
            .await
            .map(|_| ())
    })
    .await;
    assert!(matches!(result, Err(Error::Stale)));
    assert!(
        !flag(&db, &run).await,
        "결과를 내보내지 않았으니 표시도 없다"
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
    let id = create(&db, &run, "비공개 메모").await;
    assert!(!flag(&db, &run).await);
    assert_eq!(texts(&db, &run, "메모").await, ["비공개 메모"]);
    assert!(flag(&db, &run).await);
    db.sql("update bot_runs set private_memory_read = false")
        .await;
    memories(&db)
        .preview(run.attempt.clone(), id)
        .await
        .unwrap();
    assert!(flag(&db, &run).await);
}
