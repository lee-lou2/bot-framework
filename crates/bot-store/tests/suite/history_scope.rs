//! 대화 이력의 경계(PG): HTTP 대화는 호출 주체별, 예약 대화는 (원래 에이전트, 트리거, 주체)별로 갈리고,
//! private 기억을 읽은 실행의 답이 이력으로 들어오면 보호 표지가 새 실행 트리에 같은 커밋으로 따라온다.

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
    finish(&db, &a1, "A의 첫 답").await;
    let b1 = run_in(&db, "b1", "http:inbox/b", ConversationKind::Http).await;
    assert!(
        history(&db, &b1).await.is_empty(),
        "다른 호출자의 차례를 보지 못한다"
    );
    finish(&db, &b1, "B의 첫 답").await;
    let a2 = run_in(&db, "a2", "http:inbox/a", ConversationKind::Http).await;
    let seen = history(&db, &a2).await;
    assert_eq!(seen, ["hi", "A의 첫 답"], "자기 차례만");
    // 다른 입구(CLI·DM)의 대화는 원래대로 주체와 무관하게 대화 단위로 본다
    let c1 = run_in(&db, "c1", OWNER, ConversationKind::Cli).await;
    finish(&db, &c1, "CLI 답").await;
    let c2 = run_in(&db, "c2", OWNER, ConversationKind::Cli).await;
    assert_eq!(history(&db, &c2).await, ["hi", "CLI 답"]);
}

async fn scheduled(db: &TestDb, request: &str, agent: &str, trigger: &str) -> RunContext {
    db.publish().await;
    let mut a = accept_as(
        "personal",
        request,
        "service:morning",
        ConversationKind::Scheduled,
        "morning",
        "아침 점검",
    );
    a.target = Target::Agent {
        agent: AgentId(agent.into()),
        instruction: "아침 점검".into(),
    };
    runs(db).accept(a).await.unwrap();
    // 접수 경로가 달라 트리거 이름은 직접 단다(예약 접수가 하는 일)
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
    finish(&db, &first, "main의 아침 답").await;
    // 같은 이름의 다른 에이전트·다른 트리거는 섞이지 않는다
    let other_agent = scheduled(&db, "s2", "other", "morning").await;
    assert!(history(&db, &other_agent).await.is_empty());
    finish(&db, &other_agent, "other의 아침 답").await;
    let other_trigger = scheduled(&db, "s3", "main", "evening").await;
    assert!(history(&db, &other_trigger).await.is_empty());
    finish(&db, &other_trigger, "main의 저녁 답").await;
    // 같은 (에이전트, 트리거, 주체)면 이어서 본다
    let again = scheduled(&db, "s4", "main", "morning").await;
    assert_eq!(history(&db, &again).await, ["아침 점검", "main의 아침 답"]);
}

#[tokio::test]
async fn an_answer_that_came_from_private_memory_carries_the_protection_to_the_next_run() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 비밀 기억을 읽지 않은 대화는 이력이 들어와도 표지가 서지 않는다
    let plain = run_in(&db, "p0", OWNER, ConversationKind::Cli).await;
    finish(&db, &plain, "평범한 답").await;
    let clean = run_in(&db, "p1", OWNER, ConversationKind::Cli).await;
    assert_eq!(history(&db, &clean).await, ["hi", "평범한 답"]);
    assert!(!flag(&db, &clean).await, "읽은 비밀이 없으면 표지도 없다");
    // 이 실행이 private 기억을 읽고(표지가 선다) 답한다
    create(&db, &clean, "비밀 메모").await;
    assert_eq!(texts(&db, &clean, "비밀").await, ["비밀 메모"]);
    assert!(flag(&db, &clean).await);
    finish(&db, &clean, "비밀 메모를 담은 답").await;

    // 다음 실행의 이력에 그 답이 들어오면 돌려주기 전에 이 실행 트리에 표지가 같은 커밋으로 선다
    let next = run_in(&db, "p2", OWNER, ConversationKind::Cli).await;
    assert!(!flag(&db, &next).await);
    let seen = history(&db, &next).await;
    assert!(seen.contains(&"비밀 메모를 담은 답".to_owned()));
    assert!(flag(&db, &next).await, "보호 표지가 따라왔다");
}
