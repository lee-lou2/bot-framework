//! OpenAI-compatible HTTP run status lookup: wait kinds, final text/usage, stored approval blocks, and isolation from legacy webhook paths.

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
    open_local(&db, &ctx.attempt, "c1", "n1").await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
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
            done("long reply"),
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
            text: "long reply".into(),
            truncated: true,
            usage: Some(TokenUsage {
                prompt: 11,
                completion: 5
            })
        }
    );
    let plain = accepted(admit(&db, request("k2", "build", "assistant", 2, 8)).await);
    let ctx = claimed(&db).await;
    assert_eq!(ctx.attempt.run, plain.run);
    runs(&db)
        .finish(ctx.attempt.clone(), done("short reply"))
        .await
        .unwrap();
    assert_eq!(
        lookup(&db, "k2", "build").await.unwrap().state,
        ApiState::Final {
            text: "short reply".into(),
            truncated: false,
            usage: None
        }
    );
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
                    result: JsonPayload(r#"{"text":"all done"}"#.into()),
                    answers: vec![],
                },
            )
            .await
            .unwrap();
        // A stored block verdict takes precedence over model completion text.
        assert_eq!(
            lookup(&db, &key, "build").await.unwrap().state,
            ApiState::ApprovalBlocked(why)
        );
    }
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
    // An open human approval takes precedence over a prior block verdict.
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
    db.store
        .delegate(Delegate {
            parent: parent.attempt.clone(),
            checked_revision: Revision("r1".into()),
            step: "research".into(),
            child: AgentId("main/research".into()),
            instruction: "investigate".into(),
            scopes: scopes(&["run"]),
        })
        .await
        .unwrap();
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::Active
    );
    let child = claimed(&db).await;
    assert_ne!(child.attempt.run, run.run);
    assert_eq!(child.root, run.run, "child inherits root origin");
    open_local(&db, &child.attempt, "c1", "n1").await;
    assert_eq!(
        lookup(&db, "k1", "build").await.unwrap().state,
        ApiState::NeedsHuman
    );
    // Slack DM cards are not actionable until the delivery receipt is Posted.
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
    // Simulate redeploy switching this binding from webhook to the OpenAI API endpoint.
    db.sql("update bot_requests set api_endpoint = 'openai', api_model = 'assistant'")
        .await;
    db.sql("update bot_runs set api_endpoint = 'openai'").await;
    assert!(runs(&db).status(key).await.unwrap().is_none());
    let got = runs(&db).accept(legacy).await;
    assert!(matches!(got, Err(Error::Conflict)), "{got:?}");
    assert_eq!(db.scalar::<i64>("select count(*) from bot_runs").await, 1);
    let seen = lookup(&db, "same-key", "build").await.expect("API lookup");
    assert_eq!((seen.run, seen.binding.0.as_str()), (run, "hook"));
}
