mod support;

use bot_core::{command::*, id::*, port::*, state::*};
use support::*;

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

/// 부모(main)를 접수·집은 뒤 자식에게 위임한다. (부모 컨텍스트, 자식 run)을 돌려준다.
async fn delegated(db: &TestDb) -> (RunContext, RunId) {
    let parent_run = db
        .store
        .accept(accept("personal", "r1", "조사해줘"))
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
            "일정 맥락을 조사해",
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
        "대기 중인 부모의 리스는 놓였다"
    );

    // 자식은 같은 요청 트리·주체·대화를 이어받고 권한은 좁아진다
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
        "부모는 자식이 끝나기 전에 집히지 않는다"
    );

    db.store
        .finish(got.attempt.clone(), done("조사 결과"))
        .await
        .unwrap();
    assert_eq!(state_of(&db, &parent.attempt.run).await, "ready");
    let resumed = db.store.claim().await.unwrap().unwrap();
    assert_resumed(&parent.attempt, &resumed.attempt);
    let step = &resumed.work.steps[0];
    assert_eq!(step.key, "1");
    assert_eq!(
        step.body.0,
        r#"{"result":{"text":"조사 결과"},"state":"succeeded"}"#
    );

    // 같은 자식 완료를 다시 보내도 부모는 두 번 깨지 않는다
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
    // 재개한 부모가 같은 단계를 다시 요청하면 같은 자식이다(새 자식·새 대기 없음)
    let again = db
        .store
        .delegate(delegate(
            &resumed.attempt,
            "1",
            "main/research",
            "일정 맥락을 조사해",
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
            "다른 지시",
            &["run"],
        ))
        .await;
    assert!(matches!(changed, Err(Error::Conflict)));
    // 좁힌 권한이 달라져도 같은 단계의 재요청은 충돌이다
    let wider = db
        .store
        .delegate(delegate(
            &resumed.attempt,
            "1",
            "main/research",
            "일정 맥락을 조사해",
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

    // 취소: 두 번째 위임을 취소하면 부모가 취소 결과로 깨어난다
    let second = db
        .store
        .delegate(delegate(
            &resumed.attempt,
            "2",
            "main/research",
            "다시",
            &["run"],
        ))
        .await
        .unwrap();
    db.store
        .cancel(
            profile(),
            second,
            PrincipalId("local:owner".into()),
            "그만".into(),
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

    // 운영자가 dead letter를 재개하고 이번엔 성공한다: 부모는 이미 결과를 받았으므로 다시 깨지 않는다
    db.store
        .resume(
            profile(),
            child.clone(),
            PrincipalId("local:owner".into()),
            "고침".into(),
        )
        .await
        .unwrap();
    let retry = db.store.claim().await.unwrap().unwrap();
    assert_eq!(retry.attempt.run, child);
    db.store
        .finish(retry.attempt, done("이번엔 성공"))
        .await
        .unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_steps").await,
        1,
        "체크포인트는 한 번뿐"
    );
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.wake'")
            .await,
        1
    );
    assert_eq!(
        state_of(&db, &parent.attempt.run).await,
        "leased",
        "부모는 계속 자기 소유자가 쥔다"
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
        ("main/research/deep", &["run"][..]), // 직계가 아님
        ("main", &["run"][..]),               // 자기 자신
        ("other/research", &["run"][..]),     // 다른 계보
        ("main/research", &["run", "calendar.delete"][..]), // 부모에게 없는 권한
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
        "거절된 위임은 부모를 건드리지 않는다"
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

    // 취소 요청이 선 부모는 새 일을 맡기지 못한다
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
            "그만".into(),
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
    db.store.finish(got.attempt, done("자식 답")).await.unwrap();
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_outbox").await,
        0,
        "부모만 답한다"
    );
    let resumed = db.store.claim().await.unwrap().unwrap();
    db.store
        .finish(resumed.attempt, done("부모 답"))
        .await
        .unwrap();
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 1);
}
