use super::*;
use bot_broker::seal::JcsSealer;
use bot_profile::SchemaValidator;

async fn refusal_before_approval(kind: ApiKind) {
    let Some(w) = world_with(Arc::new(JcsSealer::new(Arc::new(SchemaValidator::new())))).await
    else {
        return;
    };
    let api = FakeApi::start().await;
    api.script(calls(
        kind,
        "",
        vec![
            Call {
                id: "bad",
                ..call("agenda.move", json!({"typo": "e1"}))
            },
            Call {
                id: "approval",
                ..call("agenda.delete", json!({"event": "e1"}))
            },
        ],
        None,
    ));
    api.script(text(kind, "확인했어요", None));
    let run = w.accept("refusal", "일정 정리").await;
    let worker = w.worker(&w.pg.store, models(kind, &api), config());
    let invalid = "select count(*) from bot_audit where action = 'call.invalid'";
    assert_eq!(worker.run_once().await, Some(Outcome::Parked));
    assert_eq!(w.pg.scalar::<i64>(invalid).await, 1);
    let nonce =
        w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
            .await;
    w.broker
        .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), false)
        .await
        .unwrap();
    assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
    assert_eq!(
        w.pg.scalar::<i64>(invalid).await,
        1,
        "재개 때 앞선 거절을 다시 판정했다"
    );
    let id = bot_runtime::model::call_id(&run, 0, 0);
    assert_eq!(
        view::marks(&w, &run).await,
        [(bot_core::cli::CliRefusal::key(&id), r#""invalid""#.into())]
    );
    let seen = api.seen();
    assert_eq!(seen.len(), 2);
    let request = seen[1].json();
    match kind {
        ApiKind::Anthropic => {
            assert_eq!(
                view::result_text(&request, "t0c0").as_deref(),
                Some("invalid")
            );
            assert_eq!(view::result_error(&request, "t0c0"), Some(true));
        }
        ApiKind::OpenAi => {
            let result = request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["tool_call_id"] == "bad")
                .unwrap();
            assert_eq!(result["content"], "invalid");
        }
    }
    assert!(w.runner.jobs().is_empty());
}

#[tokio::test]
async fn anthropic_replays_a_refusal_before_an_approval_wait() {
    refusal_before_approval(ApiKind::Anthropic).await;
}

#[tokio::test]
async fn openai_replays_a_refusal_before_an_approval_wait() {
    refusal_before_approval(ApiKind::OpenAi).await;
}

#[tokio::test]
async fn a_refusal_that_could_not_be_saved_is_judged_again() {
    let Some(w) = world_with(Arc::new(JcsSealer::new(Arc::new(SchemaValidator::new())))).await
    else {
        return;
    };
    let api = FakeApi::start().await;
    api.script(calls(
        ApiKind::Anthropic,
        "",
        vec![call("agenda.move", json!({}))],
        None,
    ));
    api.script(text(ApiKind::Anthropic, "확인했어요", None));
    let run = w.accept("retry-refusal", "일정 정리").await;
    let worker = w.worker(&w.pg.store, models(ApiKind::Anthropic, &api), config());
    w.pg.sql("create function fail_mark() returns trigger language plpgsql as $$ begin raise sqlstate '08006'; end $$").await;
    w.pg.sql("create trigger fail_mark before insert on bot_steps for each row when (new.key like 'cli-refusal-%') execute function fail_mark()").await;
    assert!(matches!(
        worker.run_once().await,
        Some(Outcome::Failed(Retry::After(_)))
    ));
    assert!(view::marks(&w, &run).await.is_empty());
    assert_eq!(api.seen().len(), 1);
    w.pg.sql("drop trigger fail_mark on bot_steps").await;
    w.pg.sql("update bot_runs set not_before_ms = 0 where state = 'retry'")
        .await;
    assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
    assert_eq!(view::marks(&w, &run).await.len(), 1);
    assert_eq!(
        w.pg.scalar::<i64>("select count(*) from bot_audit where action = 'call.invalid'")
            .await,
        2
    );
    assert!(w.runner.jobs().is_empty());
}
