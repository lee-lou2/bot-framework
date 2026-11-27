//! 키로 이어지는 긴 요청 입구(OpenAI 호환 HTTP)의 상태 조회(PG): 실제 대기 종류·저장된 최종 글·잘림·사용량·실패, 저장된 승인 막힘 판정,
//! 사람이 지금 결정할 수 있는 카드가 있을 때만 승인 대기, 일반(webhook) 접수·상태 경로와의 분리.

use crate::support::{api::*, *};
use bot_core::{
    api::{ApiState, ApprovalBlock},
    command::{Delegate, Failure, Finish, JsonPayload, RunOutcome, TokenUsage},
    id::*,
    ledger::{FinishCall, Outcome},
    port::{DelegationStore, Error},
    state::*,
};

#[tokio::test]
async fn the_status_reports_the_real_wait_kind_the_stored_result_and_the_failure() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = accepted(admit(&db, request("k1", "build", "assistant", 1, 8)).await);
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    let ctx = claimed(&db).await;
    assert_eq!(ctx.attempt.run, run.run);
    // 실제 승인 대기: 승인을 열면 run이 승인 대기로 넘어간다
    open_local(&db, &ctx.attempt, "c1", "n1").await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    // 보류·자식 대기는 «승인 대기»가 아니다
    db.sql(&format!(
        "update bot_runs set wait_kind = 'hold' where id = '{}'",
        run.run.0
    ))
    .await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Held
    );
    db.sql(&format!(
        "update bot_runs set wait_kind = 'child', wait_child = 'x' where id = '{}'",
        run.run.0
    ))
    .await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    // 성공: 저장된 최종 글·잘림·사용량(계량을 쓰지 않은 완료는 «잘리지 않음·사용량 모름»)
    db.sql(&format!(
        "update bot_runs set state = 'ready', wait_kind = null, wait_child = null, keep_generation = true where id = '{}'",
        run.run.0
    ))
    .await;
    db.sql("update bot_approvals set state = 'denied'").await;
    let ctx = claimed(&db).await;
    let done = |text: &str| Finish {
        result: JsonPayload(serde_json::json!({ "text": text }).to_string()),
        answers: vec![],
    };
    runs(&db)
        .finish_with(
            ctx.attempt.clone(),
            done("긴 답"),
            RunOutcome {
                truncated: true,
                usage: Some(TokenUsage {
                    prompt: 11,
                    completion: 5,
                }),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Final {
            text: "긴 답".into(),
            truncated: true,
            usage: Some(TokenUsage {
                prompt: 11,
                completion: 5
            })
        }
    );
    // 계량 없는 완료(`finish`)는 지어내지 않는다
    let plain = accepted(admit(&db, request("k2", "build", "assistant", 2, 8)).await);
    let ctx = claimed(&db).await;
    assert_eq!(ctx.attempt.run, plain.run);
    runs(&db)
        .finish(ctx.attempt.clone(), done("짧은 답"))
        .await
        .unwrap();
    assert_eq!(
        lookup(&db, "k2", "build").await.unwrap().state,
        ApiState::Final {
            text: "짧은 답".into(),
            truncated: false,
            usage: None
        }
    );
    // 실패와 취소는 고정 사유다
    let failed = accepted(admit(&db, request("k3", "build", "assistant", 3, 8)).await);
    let ctx = claimed(&db).await;
    assert_eq!(ctx.attempt.run, failed.run);
    runs(&db)
        .fail(ctx.attempt.clone(), Failure::Permanent("model_error"))
        .await
        .unwrap();
    assert_eq!(
        lookup(&db, "k3", "build").await.unwrap().state,
        ApiState::Failed {
            reason: "model_error".into()
        }
    );
    let cancelled = accepted(admit(&db, request("k4", "build", "assistant", 4, 8)).await);
    db.sql(&format!(
        "update bot_runs set state = 'cancelled' where id = '{}'",
        cancelled.run.0
    ))
    .await;
    assert_eq!(
        lookup(&db, "k4", "build").await.unwrap().state,
        ApiState::Cancelled
    );
}

#[tokio::test]
async fn a_stored_approval_block_replaces_the_model_text_but_never_a_pending_approval() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    for why in ApprovalBlock::ALL.into_iter().take(3) {
        let key = format!("k-{}", why.code());
        let run = accepted(admit(&db, request(&key, "build", "assistant", 1, 8)).await);
        let ctx = claimed(&db).await;
        assert_eq!(ctx.attempt.run, run.run);
        // 승인 경로가 없어 도구 호출이 거절로 닫혔다: 저장된 낱말이 조회에 나온다(진행 중에도, 모델이 글을 내도)
        begin_call(&db, &ctx.attempt, "c1").await;
        calls(&db)
            .finish_blocked(
                FinishCall {
                    attempt: ctx.attempt.clone(),
                    call: call("c1"),
                    outcome: Outcome::Blocked,
                },
                why,
            )
            .await
            .unwrap();
        assert_eq!(
            lookup(&db, &key, "build").await.unwrap().state,
            ApiState::ApprovalBlocked(why),
            "{}",
            why.code()
        );
        runs(&db)
            .finish(
                ctx.attempt.clone(),
                Finish {
                    result: JsonPayload(r#"{"text":"다 했어요"}"#.into()),
                    answers: vec![],
                },
            )
            .await
            .unwrap();
        // 성공한 run도 모델이 «다 했다»고 쓴 글을 내보내지 않는다
        assert_eq!(
            lookup(&db, &key, "build").await.unwrap().state,
            ApiState::ApprovalBlocked(why)
        );
    }
    // 이유 없는 일반 거절(`finish`)은 막힘 판정이 아니다
    let plain = accepted(admit(&db, request("plain", "build", "assistant", 2, 8)).await);
    let ctx = claimed(&db).await;
    assert_eq!(ctx.attempt.run, plain.run);
    begin_call(&db, &ctx.attempt, "c1").await;
    calls(&db)
        .finish(FinishCall {
            attempt: ctx.attempt.clone(),
            call: call("c1"),
            outcome: Outcome::Blocked,
        })
        .await
        .unwrap();
    assert_eq!(
        lookup(&db, "plain", "build").await.unwrap().state,
        ApiState::Active
    );
    // 사람의 승인을 아직 기다리는 중이면 막힘으로 덮지 않는다
    begin_call(&db, &ctx.attempt, "c2").await;
    calls(&db)
        .finish_blocked(
            FinishCall {
                attempt: ctx.attempt.clone(),
                call: call("c2"),
                outcome: Outcome::Blocked,
            },
            ApprovalBlock::NoApprover,
        )
        .await
        .unwrap();
    open_local(&db, &ctx.attempt, "c3", "n3").await;
    assert_eq!(
        lookup(&db, "plain", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    // 이유를 저장하지 못하는 호출 닫기는 이유를 붙일 수 없다
    let err = calls(&db)
        .finish_blocked(
            FinishCall {
                attempt: ctx.attempt.clone(),
                call: call("c3"),
                outcome: Outcome::Released,
            },
            ApprovalBlock::NoApprover,
        )
        .await;
    assert!(matches!(err, Err(Error::Conflict)), "{err:?}");
}

#[tokio::test]
async fn a_pending_decision_anywhere_in_the_tree_is_needs_human_only_when_a_human_can_decide_it() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let run = accepted(admit(&db, request("k1", "build", "assistant", 1, 8)).await);
    let parent = claimed(&db).await;
    assert_eq!(parent.attempt.run, run.run);
    // 루트가 위임 자식을 기다린다: 자식 대기만으로는 승인 대기가 아니다
    db.store
        .delegate(Delegate {
            parent: parent.attempt.clone(),
            checked_revision: Revision("r1".into()),
            step: "research".into(),
            child: AgentId("main/research".into()),
            instruction: "조사해".into(),
            scopes: scopes(&["run"]),
        })
        .await
        .unwrap();
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    // 자식이 승인 대기에 들어간다: 로컬 승인은 카드가 곧바로 결정 가능하다
    let child = claimed(&db).await;
    assert_ne!(child.attempt.run, run.run);
    assert_eq!(child.root, run.run, "자식은 루트의 접수 출처를 잇는다");
    open_local(&db, &child.attempt, "c1", "n1").await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    // Slack DM 전달 작업이 쌓이는 중이거나 올라가는 중이면 아직 결정할 카드가 없다(영수증이 확정된 뒤에야 열린다)
    db.sql(&format!(
        "insert into bot_card_jobs (nonce, profile, run, binding, owner, generation)
         values ('n1', 'personal', '{}', 'team', 'slack:home/U1', {})",
        child.attempt.run.0, child.attempt.generation.0
    ))
    .await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    db.sql("update bot_card_jobs set state = 'sending', sending_ms = bot_now_ms(), activation = 'set-1', checked_revision = 'r1'")
        .await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    db.sql("update bot_card_jobs set state = 'posted', team = 'T1', dm = 'D1', ts = '1.5', posted_ms = bot_now_ms()")
        .await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    // 이미 결정했거나 만료한 카드, 세대가 어긋난 카드는 사람이 할 일이 아니다
    db.sql("update bot_approvals set expires_ms = bot_now_ms() - 1")
        .await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    db.sql("update bot_approvals set expires_ms = bot_now_ms() + 3600000")
        .await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    db.sql("update bot_approvals set generation = generation + 1")
        .await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    db.sql("update bot_approvals set generation = generation - 1, state = 'approved'")
        .await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
}

#[tokio::test]
async fn the_legacy_accept_and_status_paths_never_read_or_continue_an_api_row() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    // 일반(webhook) 접수를 하나 만든 뒤 그 행을 입구 접수 행으로 바꿔 «재배포로 입구 종류가 바뀐» 상황을 만든다
    let mut legacy = accept_as(
        "personal",
        "same-key",
        &principal("build").0,
        ConversationKind::Http,
        "c1",
        "hi",
    );
    legacy.event.binding = BindingId("hook".into());
    let run = runs(&db).accept(legacy.clone()).await.unwrap();
    let key = bot_core::intake::RequestKey {
        profile: ProfileId("personal".into()),
        binding: BindingId("hook".into()),
        principal: principal("build"),
        request: RequestId("same-key".into()),
    };
    assert_eq!(
        runs(&db).status(key.clone()).await.unwrap().unwrap().run,
        run
    );
    assert_eq!(runs(&db).accept(legacy.clone()).await.unwrap(), run);
    db.sql("update bot_requests set api_endpoint = 'openai', api_model = 'assistant'")
        .await;
    db.sql("update bot_runs set api_endpoint = 'openai'").await;
    // 같은 접수 명령(같은 지문)이어도 일반 경로는 입구 접수 행을 읽지도 이어받지도 않는다
    assert!(runs(&db).status(key).await.unwrap().is_none());
    let got = runs(&db).accept(legacy).await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    // 입구 조회는 이 행을 자기 행으로 읽는다(원래 모델·바인딩은 접수 때 고정한 값이다)
    let seen = lookup(&db, "same-key", "build").await.expect("입구 조회");
    assert_eq!((seen.run, seen.binding.0.as_str()), (run, "hook"));
}
