use super::*;
use bot_core::{command::JsonPayload, id::ToolId};
use bot_runtime::model::{CallEnd, DoneCall, Exchange, call_id};

#[tokio::test]
async fn oversize_plan_is_permanent_without_a_second_model_request() {
    for kind in KINDS {
        let Some(w) = world().await else { return };
        let api = FakeApi::start().await;
        let args = json!({"label": "a".repeat(220 * 1024)});
        let batch = ["c1", "c2", "c3", "c4", "c5"].map(|id| Call {
            id,
            ..call("agenda.list", args.clone())
        });
        api.script(calls(kind, "", batch.into(), None));
        let run = w.accept("large-plan", "일정 조회").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Failed(Retry::Dead)));
        assert_eq!(run_error(&w, &run).await, "invalid_checkpoint");
        assert_eq!(worker.run_once().await, None);
        assert_eq!(api.seen().len(), 1);
        assert!(w.runner.jobs().is_empty());
        assert_eq!(
            w.pg.scalar::<i64>("select count(*) from bot_steps").await,
            0
        );
    }
}

#[tokio::test]
async fn the_result_checkpoint_accepts_one_mib_and_rejects_one_more_byte_after_one_write() {
    const LIMIT: usize = 1 << 20;
    let kind = ApiKind::Anthropic;
    for bytes in [LIMIT, LIMIT + 1] {
        let Some(w) = world().await else { return };
        let run = w.accept("large-result", "일정 삭제").await;
        let api = FakeApi::start().await;
        let tool = "agenda.delete";
        let args = json!({"event": "e1"});
        let assistant = "a".repeat(900 * 1024);
        api.script(calls(
            kind,
            &assistant,
            vec![call(tool, args.clone())],
            None,
        ));
        api.script(text(kind, "완료", None));
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Parked));
        // 실제 교환 형식의 고정 부분(공급자 상태 포함)을 재서 도구 결과를 정확한 경계까지 채운다.
        let plan: serde_json::Value = serde_json::from_str(
            &w.pg
                .scalar::<String>("select body from bot_steps where key = 'turn-0'")
                .await,
        )
        .unwrap();
        let empty = Exchange {
            text: assistant,
            calls: vec![DoneCall {
                call: call_id(&run, 0, 0),
                tool: ToolId(tool.into()),
                args: JsonPayload(args.to_string()),
                end: CallEnd::Done(JsonPayload(r#"{"result":""}"#.into())),
            }],
            state: plan["state"].as_str().map(str::to_owned),
        }
        .to_payload();
        w.runner
            .set_default_result(&json!({"result": "r".repeat(bytes - empty.0.len())}).to_string());
        let nonce =
            w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
                .await;
        w.broker
            .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
            .await
            .unwrap();
        if bytes == LIMIT {
            assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
            assert_eq!(
                w.pg.scalar::<i32>("select octet_length(body) from bot_steps where key = 'end-0'")
                    .await,
                LIMIT as i32
            );
            assert_eq!(api.seen().len(), 2);
        } else {
            assert_eq!(worker.run_once().await, Some(Outcome::Failed(Retry::Dead)));
            assert_eq!(run_error(&w, &run).await, "invalid_checkpoint");
            assert_eq!(api.seen().len(), 1);
        }
        assert_eq!(worker.run_once().await, None);
        assert_eq!(w.runner.jobs().len(), 1);
        assert_eq!(
            w.pg.scalar::<i64>("select count(*) from bot_calls where state = 'done'")
                .await,
            1
        );
    }
}

#[tokio::test]
async fn an_unavailable_result_checkpoint_retries_using_the_committed_call() {
    let Some(w) = world().await else { return };
    let kind = ApiKind::Anthropic;
    let api = FakeApi::start().await;
    api.script(calls(kind, "", vec![call("agenda.list", json!({}))], None));
    api.script(text(kind, "완료", None));
    let run = w.accept("store-outage", "일정 조회").await;
    let worker = w.worker(&w.pg.store, models(kind, &api), config());
    // 계획·도구 영수증은 커밋되고 결과 체크포인트 쓰기만 저장소 장애로 롤백된다.
    w.pg.sql("create function fail_end() returns trigger language plpgsql as $$ begin raise sqlstate '08006'; end $$").await;
    w.pg.sql("create trigger fail_end before insert on bot_steps for each row when (new.key = 'end-0') execute function fail_end()").await;
    assert!(matches!(
        worker.run_once().await,
        Some(Outcome::Failed(Retry::After(_)))
    ));
    assert_eq!(run_error(&w, &run).await, "checkpoint_unavailable");
    w.pg.sql("drop trigger fail_end on bot_steps").await;
    w.pg.sql("update bot_runs set not_before_ms = 0 where state = 'retry'")
        .await;
    assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
    assert_eq!(w.runner.jobs().len(), 1);
    assert_eq!(api.seen().len(), 2);
}
