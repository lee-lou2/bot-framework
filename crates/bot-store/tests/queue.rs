mod support;

use bot_core::{command::*, id::*, port::*, state::*};
use bot_store::Config;
use std::time::Duration;
use support::*;

fn stale<T: std::fmt::Debug>(result: Result<T, Error>) -> bool {
    matches!(result, Err(Error::Stale))
}

async fn accepted(db: &TestDb, request: &str) -> RunId {
    db.store
        .accept(accept("personal", request, "hi"))
        .await
        .unwrap()
}

async fn state_of(db: &TestDb, run: &RunId) -> String {
    db.scalar::<String>(&format!(
        "select state from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

#[tokio::test]
async fn claims_oldest_first_and_raises_the_generation() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut runs = Vec::new();
    for n in 0..3 {
        let run = accepted(&db, &format!("req-{n}")).await;
        db.sql(&format!(
            "update bot_runs set created_ms = {} where id = '{}'",
            1000 + n,
            run.0
        ))
        .await;
        runs.push(run);
    }
    let mut claimed = Vec::new();
    for run in &runs {
        let got = db.store.claim().await.unwrap().unwrap();
        claimed.push(got.attempt.clone());
        assert_eq!(&got.attempt.run, run);
        assert_eq!(got.attempt.generation, Generation(1));
        assert_eq!(got.root, got.attempt.run, "루트 run은 자기 자신이 뿌리");
        assert_eq!(got.principal.0, "local:owner");
        assert_eq!(got.agent.0, "main");
        assert_eq!(got.conversation.kind, ConversationKind::Cli);
        assert_eq!(got.scopes, run_scopes());
        assert!(!got.private_memory_read);
        assert_eq!(got.work.revision.0, "r1");
        assert!(got.work.steps.is_empty());
    }
    assert!(db.store.claim().await.unwrap().is_none(), "줄이 비면 None");

    // 실패 뒤 다시 집으면 세대가 오른다
    let first = &runs[0];
    let attempt = claimed[0].clone();
    assert_eq!((&attempt.run, attempt.generation), (first, Generation(1)));
    assert!(matches!(
        db.store
            .fail(attempt.clone(), Failure::Transient("overloaded"))
            .await
            .unwrap(),
        Retry::After(_)
    ));
    db.make_due(first).await;
    assert_eq!(
        db.store.claim().await.unwrap().unwrap().attempt.generation,
        Generation(2)
    );
}

#[tokio::test]
async fn rejects_writes_from_expired_owner() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let worker_b = db.peer("worker-b").await;
    let run = accepted(&db, "req-fence").await;
    let old = db.store.claim().await.unwrap().unwrap();
    assert!(
        db.store.renew(old.attempt.clone()).await.is_ok(),
        "살아 있는 리스는 갱신된다"
    );

    db.expire_lease(&run).await;
    assert!(
        stale(db.store.renew(old.attempt.clone()).await),
        "리스가 끝나면 회수 전이라도 거절"
    );
    let new = worker_b.claim().await.unwrap().unwrap();
    assert_eq!(new.attempt.generation, Generation(2));
    assert_eq!(new.attempt.run, run);

    // 옛 소유자의 모든 쓰기는 새 소유자를 덮지 못한다
    assert!(stale(db.store.renew(old.attempt.clone()).await));
    let step = Step {
        key: "model-1".into(),
        body: JsonPayload("{}".into()),
    };
    assert!(stale(db.store.checkpoint(old.attempt.clone(), step).await));
    assert!(stale(
        db.store
            .fail(old.attempt.clone(), Failure::Permanent("bug"))
            .await
    ));
    assert!(stale(
        db.store.finish(old.attempt.clone(), done("late")).await
    ));
    assert!(stale(db.store.history(old.attempt.clone(), 5).await));
    assert_eq!(state_of(&db, &run).await, "leased");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 0);

    worker_b
        .finish(new.attempt.clone(), done("fresh"))
        .await
        .unwrap();
    assert_eq!(state_of(&db, &run).await, "succeeded");
    assert_eq!(
        db.scalar::<String>("select result from bot_runs").await,
        r#"{"text":"fresh"}"#
    );
}

#[tokio::test]
async fn lease_expiry_consumes_attempts_then_dead_letters() {
    let Some(db) = TestDb::with(Config {
        max_attempts: 2,
        ..Config::default()
    })
    .await
    else {
        return;
    };
    let run = accepted(&db, "req-poison").await;
    db.store.claim().await.unwrap().unwrap();
    db.expire_lease(&run).await;
    let second = db.store.claim().await.unwrap().unwrap();
    assert_eq!(
        second.attempt.generation,
        Generation(2),
        "인계는 세대를 올린다"
    );
    assert_eq!(db.scalar::<i32>("select attempts from bot_runs").await, 1);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.reclaim'")
            .await,
        1
    );

    db.expire_lease(&run).await;
    assert!(
        db.store.claim().await.unwrap().is_none(),
        "한도에 닿으면 집지 않고 dead letter"
    );
    assert_eq!(state_of(&db, &run).await, "dead");
    assert_eq!(
        db.scalar::<String>("select error from bot_runs").await,
        "lease_expired"
    );
}

#[tokio::test]
async fn retries_with_backoff_then_dead_letters() {
    let cfg = Config {
        max_attempts: 3,
        backoff_base: Duration::from_secs(10),
        backoff_cap: Duration::from_secs(300),
        ..Config::default()
    };
    let Some(db) = TestDb::with(cfg).await else {
        return;
    };
    let run = accepted(&db, "req-retry").await;
    let mut generation = 0;
    for (failures, (low, high)) in [(7.5, 12.5), (15.0, 25.0)].into_iter().enumerate() {
        let got = db.store.claim().await.unwrap().unwrap();
        generation += 1;
        assert_eq!(got.attempt.generation, Generation(generation));
        let Retry::After(wait) = db
            .store
            .fail(got.attempt.clone(), Failure::Transient("overloaded"))
            .await
            .unwrap()
        else {
            panic!("재시도여야 한다 (실패 {})", failures + 1)
        };
        let secs = wait.as_secs_f64();
        assert!((low..=high).contains(&secs), "지수 백오프 ±25%: {secs}");
        assert_eq!(state_of(&db, &run).await, "retry");
        assert!(
            db.store.claim().await.unwrap().is_none(),
            "대기 시간 전에는 집지 않는다"
        );
        db.make_due(&run).await;
    }
    let last = db.store.claim().await.unwrap().unwrap();
    assert_eq!(
        db.store
            .fail(last.attempt.clone(), Failure::Transient("overloaded"))
            .await
            .unwrap(),
        Retry::Dead
    );
    assert_eq!(state_of(&db, &run).await, "dead");
    assert_eq!(
        db.scalar::<String>("select error from bot_runs").await,
        "overloaded"
    );
}

#[tokio::test]
async fn permanent_failure_goes_dead_at_once() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = accepted(&db, "req-perm").await;
    let got = db.store.claim().await.unwrap().unwrap();
    assert_eq!(
        db.store
            .fail(got.attempt.clone(), Failure::Permanent("invalid_output"))
            .await
            .unwrap(),
        Retry::Dead
    );
    assert_eq!(state_of(&db, &run).await, "dead");
    assert!(db.store.claim().await.unwrap().is_none());
    assert!(
        stale(db.store.finish(got.attempt.clone(), done("x")).await),
        "끝난 run에는 더 쓸 수 없다"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn claims_each_run_once_under_concurrency() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let mut expected = Vec::new();
    for n in 0..24 {
        expected.push(accepted(&db, &format!("req-{n}")).await);
    }
    let mut tasks = Vec::new();
    for n in 0..6 {
        let store = db.peer(&format!("worker-{n}")).await;
        tasks.push(tokio::spawn(async move {
            let mut mine = Vec::new();
            while let Some(got) = store.claim().await.unwrap() {
                mine.push(got.attempt.run);
            }
            mine
        }));
    }
    let mut claimed = Vec::new();
    for t in tasks {
        claimed.extend(t.await.unwrap());
    }
    claimed.sort_by(|a, b| a.0.cmp(&b.0));
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(claimed, expected, "모든 run이 정확히 한 번씩 집힌다");
}

#[tokio::test]
async fn checkpoint_is_idempotent_and_rejects_changed_content() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    accepted(&db, "req-cp").await;
    let got = db.store.claim().await.unwrap().unwrap();
    let step = |body: &str| Step {
        key: "model-1".into(),
        body: JsonPayload(body.into()),
    };
    db.store
        .checkpoint(got.attempt.clone(), step(r#"{"call":"c-1"}"#))
        .await
        .unwrap();
    db.store
        .checkpoint(got.attempt.clone(), step(r#"{ "call": "c-1" }"#))
        .await
        .unwrap();
    let changed = db
        .store
        .checkpoint(got.attempt.clone(), step(r#"{"call":"c-2"}"#))
        .await;
    assert!(matches!(changed, Err(Error::Conflict)));
    assert_eq!(db.scalar::<i64>("select count(*) from bot_steps").await, 1);

    // 다시 집으면 체크포인트가 순서대로 들어 있다
    db.store
        .checkpoint(
            got.attempt.clone(),
            Step {
                key: "tool-1".into(),
                body: JsonPayload("1".into()),
            },
        )
        .await
        .unwrap();
    db.store
        .fail(got.attempt.clone(), Failure::Transient("overloaded"))
        .await
        .unwrap();
    db.make_due(&got.attempt.run).await;
    let again = db.store.claim().await.unwrap().unwrap();
    let keys: Vec<_> = again.work.steps.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(keys, ["model-1", "tool-1"]);
}

#[tokio::test]
async fn finish_records_result_and_reply_in_one_commit() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // Slack DM 같은 푸시 대화: 회신 행이 결과와 같은 커밋으로 쌓인다
    let dm = accept_as(
        "personal",
        "req-fin",
        "slack:home/U1",
        ConversationKind::Direct,
        "D1",
        "hi",
    );
    let run = db.store.accept(dm).await.unwrap();
    let got = db.store.claim().await.unwrap().unwrap();
    let two = Finish {
        result: JsonPayload(r#"{"text":"b"}"#.into()),
        answers: vec![
            JsonPayload(r#"{"text":"a"}"#.into()),
            JsonPayload(r#"{"text":"b"}"#.into()),
        ],
    };
    db.store.finish(got.attempt.clone(), two).await.unwrap();
    assert_eq!(state_of(&db, &run).await, "succeeded");
    let rows = db.scalar::<i64>("select count(*) from bot_outbox where state = 'pending' and conv_kind = 'direct' and binding = 'desk'").await;
    assert_eq!(rows, 2);
    assert_eq!(
        db.scalar::<String>("select call from bot_outbox where seq = 1")
            .await,
        format!("reply:{}:1", run.0)
    );
    assert!(
        stale(db.store.finish(got.attempt.clone(), done("again")).await),
        "같은 완료의 반복은 거절"
    );
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 2);
    assert_eq!(
        db.scalar::<i64>("select count(*) from bot_audit where action = 'run.succeeded'")
            .await,
        1
    );

    // CLI·HTTP는 결과를 상태 조회로 가져가고, 예약에는 대화가 없다: 회신 행이 없다
    for (request, kind) in [
        ("cli", ConversationKind::Cli),
        ("http", ConversationKind::Http),
        ("tick", ConversationKind::Scheduled),
    ] {
        let pull = accept_as("personal", request, "local:owner", kind, "c1", "go");
        db.store.accept(pull).await.unwrap();
        let got = db.store.claim().await.unwrap().unwrap();
        db.store.finish(got.attempt, done("x")).await.unwrap();
    }
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 2);
}

#[tokio::test]
async fn finish_rejects_malformed_replies_and_keeps_the_lease() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = db
        .store
        .accept(accept_as(
            "personal",
            "r",
            "slack:home/U1",
            ConversationKind::Direct,
            "D1",
            "hi",
        ))
        .await
        .unwrap();
    let got = db.store.claim().await.unwrap().unwrap();
    let long = format!("{{\"text\":\"{}\"}}", "가".repeat(12_001));
    let bad = [
        r#"{"approval":{"nonce":"n","title":"t","lines":[]}}"#,
        r#"{"text":"ok","extra":1}"#,
        r#"{"attachments":[]}"#,
        r#"{"attachments":[{"name":"a"}]}"#,
        r#"[1]"#,
        long.as_str(),
    ];
    for body in bad {
        let attempt = Finish {
            result: JsonPayload("{}".into()),
            answers: vec![JsonPayload(body.into())],
        };
        let result = db.store.finish(got.attempt.clone(), attempt).await;
        assert!(matches!(result, Err(Error::Invalid)), "{body}");
    }
    assert_eq!(state_of(&db, &run).await, "leased");
    let fine =
        r#"{"text":"ok","attachments":[{"name":"a.txt","type":"text/plain","data":"aGVsbG8="}]}"#;
    db.store
        .finish(
            got.attempt,
            Finish {
                result: JsonPayload("{}".into()),
                answers: vec![JsonPayload(fine.into())],
            },
        )
        .await
        .unwrap();
    assert_eq!(db.scalar::<i64>("select count(*) from bot_outbox").await, 1);
}

#[tokio::test]
async fn history_reads_only_the_same_conversation() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let ask = |request: &str, conv: &str, text: &str| {
        accept_as(
            "personal",
            request,
            "local:owner",
            ConversationKind::Cli,
            conv,
            text,
        )
    };
    let mut ids = Vec::new();
    for (request, conv, text) in [
        ("a", "owner", "첫째"),
        ("b", "other", "남의 것"),
        ("c", "owner", "둘째"),
        ("d", "owner", "지금"),
    ] {
        let id = db.store.accept(ask(request, conv, text)).await.unwrap();
        db.sql(&format!(
            "update bot_runs set created_ms = {} where id = '{}'",
            1000 + ids.len(),
            id.0
        ))
        .await;
        ids.push(id);
    }
    // 앞의 셋을 차례로 끝내고(답 = "답: <입력>"), 마지막 run의 맥락에서 이력을 읽는다
    for _ in 0..3 {
        let got = db.store.claim().await.unwrap().unwrap();
        let input = db
            .scalar::<String>(&format!(
                "select input from bot_runs where id = '{}'",
                got.attempt.run.0
            ))
            .await;
        let text = input
            .trim_start_matches("{\"text\":\"")
            .trim_end_matches("\"}")
            .to_owned();
        db.store
            .finish(got.attempt, done(&format!("답: {text}")))
            .await
            .unwrap();
    }
    let last = db.store.claim().await.unwrap().unwrap();
    assert_eq!(last.attempt.run, ids[3]);
    let turns = db.store.history(last.attempt, 10).await.unwrap();
    let texts: Vec<_> = turns.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(texts, ["첫째", "답: 첫째", "둘째", "답: 둘째"]);
    assert!(turns[0].from.is_some() && turns[1].from.is_none());
}
