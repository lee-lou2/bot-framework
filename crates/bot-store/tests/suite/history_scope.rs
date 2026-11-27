//! Conversation history isolation (PG): HTTP history splits by principal, scheduled history
//! splits by `(agent, trigger, principal)`, and private-memory taint propagates via history.

use crate::support::*;
use bot_core::{command::*, id::*, state::*};

async fn finish(db: &TestDb, run: &RunContext, text: &str) {
    runs(db)
        .finish(run.attempt.clone(), done(text))
        .await
        .unwrap();
}

async fn history(db: &TestDb, run: &RunContext) -> Vec<String> {
    runs(db)
        .history(run.attempt.clone(), 50)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.text)
        .collect()
}

#[tokio::test]
async fn an_http_conversation_is_split_by_caller() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let a1 = run_in(&db, "a1", "http:inbox/a", ConversationKind::Http).await;
    finish(&db, &a1, "A's first reply").await;
    let b1 = run_in(&db, "b1", "http:inbox/b", ConversationKind::Http).await;
    assert!(
        history(&db, &b1).await.is_empty(),
        "must not see another caller's turns"
    );
    finish(&db, &b1, "B's first reply").await;
    let a2 = run_in(&db, "a2", "http:inbox/a", ConversationKind::Http).await;
    let seen = history(&db, &a2).await;
    assert_eq!(
        seen,
        ["hi", "A's first reply"],
        "caller sees only own turns"
    );

    let c1 = run_in(&db, "c1", OWNER, ConversationKind::Cli).await;
    finish(&db, &c1, "CLI reply").await;
    let c2 = run_in(&db, "c2", OWNER, ConversationKind::Cli).await;
    assert_eq!(history(&db, &c2).await, ["hi", "CLI reply"]);
}

async fn scheduled(db: &TestDb, request: &str, agent: &str, trigger: &str) -> RunContext {
    db.publish().await;
    let mut a = accept_as(
        "personal",
        request,
        "service:morning",
        ConversationKind::Scheduled,
        "morning",
        "morning check",
    );
    a.target = Target::Agent {
        agent: AgentId(agent.into()),
        instruction: "morning check".into(),
    };
    runs(db).accept(a).await.unwrap();
    db.sql(&format!(
        "update bot_runs set trigger = '{trigger}' where trigger is null"
    ))
    .await;
    runs(db).claim().await.unwrap().unwrap()
}

#[tokio::test]
async fn a_scheduled_conversation_is_split_by_agent_trigger_and_principal() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let first = scheduled(&db, "s1", "main", "morning").await;
    finish(&db, &first, "main morning reply").await;
    let other_agent = scheduled(&db, "s2", "other", "morning").await;
    assert!(history(&db, &other_agent).await.is_empty());
    finish(&db, &other_agent, "other morning reply").await;
    let other_trigger = scheduled(&db, "s3", "main", "evening").await;
    assert!(history(&db, &other_trigger).await.is_empty());
    finish(&db, &other_trigger, "main evening reply").await;
    let again = scheduled(&db, "s4", "main", "morning").await;
    assert_eq!(
        history(&db, &again).await,
        ["morning check", "main morning reply"]
    );
}

#[tokio::test]
async fn an_answer_that_came_from_private_memory_carries_the_protection_to_the_next_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let plain = run_in(&db, "p0", OWNER, ConversationKind::Cli).await;
    finish(&db, &plain, "ordinary reply").await;
    let clean = run_in(&db, "p1", OWNER, ConversationKind::Cli).await;
    assert_eq!(history(&db, &clean).await, ["hi", "ordinary reply"]);
    assert!(
        !flag(&db, &clean).await,
        "flag stays clear without private read"
    );

    create(&db, &clean, "secret note").await;
    assert_eq!(texts(&db, &clean, "secret").await, ["secret note"]);
    assert!(flag(&db, &clean).await);
    finish(&db, &clean, "reply containing secret note").await;

    // Reading a tainted prior reply in history commits the private flag onto the new run tree.
    let next = run_in(&db, "p2", OWNER, ConversationKind::Cli).await;
    assert!(!flag(&db, &next).await);
    let seen = history(&db, &next).await;
    assert!(seen.contains(&"reply containing secret note".to_owned()));
    assert!(
        flag(&db, &next).await,
        "private protection flag must propagate"
    );
}
