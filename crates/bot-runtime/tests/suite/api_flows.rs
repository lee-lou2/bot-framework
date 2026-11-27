//! API 직접 호출 실행기(`RunnerKind::Step`)를 실제 PG 저장소·실제 BrokerService(가짜 도구 실행기)·실제 HTTP 가짜 상류 위에서 확인한다: 최종 답과 저장된 사용량, 도구 호출 왕복(브로커 경유),
//! 승인 대기 뒤 재개(사용량·공급자 서명 보존), 위임, 구판 보호, 출력 한도, 키가 어디에도 남지 않음. 두 형식(Anthropic·OpenAI 호환)을 같은 시험으로 돌린다.

#[path = "api_flows/builtins.rs"]
mod builtins;
#[path = "api_flows/checkpoints.rs"]
mod checkpoints;
#[path = "api_flows/descriptions.rs"]
mod descriptions;
#[path = "api_flows/refusal.rs"]
mod refusal;

use crate::support::{
    api::*,
    upstream::{FakeApi, Reply},
    *,
};
use bot_core::{
    command::Retry,
    id::{ApprovalId, PrincipalId, RunId},
    port::{AuditLog, Broker},
};
use bot_runtime::{
    Models, Outcome, SingleModel,
    model::{ApiKind, ApiRunner},
};
use serde_json::json;
use std::sync::Arc;

fn models(kind: ApiKind, api: &FakeApi) -> Arc<dyn Models> {
    Arc::new(SingleModel(Arc::new(api_runner(kind, api))))
}

/// 저장된 사용량 `프롬프트/출력`(없으면 `-/-`)
async fn stored_usage(w: &World, run: &RunId) -> String {
    w.pg.scalar::<String>(&format!(
        "select coalesce(usage_prompt::text, '-') || '/' || coalesce(usage_completion::text, '-') from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

async fn run_error(w: &World, run: &RunId) -> String {
    w.pg.scalar::<String>(&format!(
        "select coalesce(error, '') from bot_runs where id = '{}'",
        run.0
    ))
    .await
}

/// 이 시험의 스키마 안 모든 표에서 `needle`이 든 행의 수(행을 글로 바꿔 훑는다)
async fn sightings(w: &World, needle: &str) -> i64 {
    let tables: Vec<String> = sqlx::query_scalar(
        "select tablename::text from pg_tables where schemaname = current_schema()",
    )
    .fetch_all(w.pg.store.pool())
    .await
    .unwrap();
    assert!(tables.len() > 10, "표를 읽지 못했다: {tables:?}");
    let mut found = 0;
    for table in tables {
        found +=
            w.pg.scalar::<i64>(&format!(
                "select count(*) from \"{table}\" t where t::text like '%{needle}%'"
            ))
            .await;
    }
    found
}

#[tokio::test]
async fn a_final_answer_is_stored_with_the_usage_the_provider_reported() {
    for kind in KINDS {
        let Some(w) = world().await else {
            return;
        };
        let api = FakeApi::start().await;
        api.script(text(kind, "일정이 없어요", Some((9, 4))));
        let run = w.accept("req-final", "오늘 일정 알려줘").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}"
        );
        let state = w.status("req-final").await;
        assert!(state.result.unwrap().0.contains("일정이 없어요"));
        assert_eq!(stored_usage(&w, &run).await, "9/4", "{kind:?}");
        assert_eq!(api.seen().len(), 1);
        assert!(w.runner.jobs().is_empty());
        // 보고하지 않는 상류는 지어내지 않는다
        let quiet = FakeApi::start().await;
        quiet.script(text(kind, "조용한 답", None));
        let run = w.accept("req-quiet", "다시 알려줘").await;
        let worker = w.worker(&w.pg.store, models(kind, &quiet), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
        assert_eq!(stored_usage(&w, &run).await, "-/-", "{kind:?}");
    }
}

#[tokio::test]
async fn a_tool_call_goes_through_the_real_broker_and_its_result_reaches_the_next_request() {
    for kind in KINDS {
        let Some(w) = world().await else {
            return;
        };
        let api = FakeApi::start().await;
        api.script(calls(
            kind,
            "조회할게요",
            vec![call("agenda.list", json!({}))],
            Some((5, 2)),
        ));
        api.script(text(kind, "산책 일정이 있어요", Some((9, 4))));
        let run = w.accept("req-tool", "오늘 일정 알려줘").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}"
        );
        assert_eq!(w.runner.jobs().len(), 1, "도구는 브로커를 거쳐 한 번");
        let seen = api.seen();
        assert_eq!(seen.len(), 2);
        // 도구 결과(비신뢰 입력)는 다음 요청의 대화에만 실렸다: 시스템 자리에는 없다
        let second = seen[1].json();
        assert!(second["messages"].to_string().contains("산책"), "{kind:?}");
        let system = match kind {
            ApiKind::Anthropic => second["system"].to_string(),
            ApiKind::OpenAi => second["messages"][0].to_string(),
        };
        assert!(
            !system.contains("산책"),
            "{kind:?}: 도구 결과가 시스템 글에 올랐다"
        );
        assert!(state_text(&w).await.contains("산책 일정이 있어요"));
        // 모든 모델 요청이 보고됐으니 합이다
        assert_eq!(stored_usage(&w, &run).await, "14/6", "{kind:?}");
        assert_eq!(
            w.spans
                .spans()
                .iter()
                .filter(|s| s.name == "worker.model")
                .count(),
            2
        );
        assert_eq!(w.pg.store.verify().await.unwrap().first_bad, None);
    }
}

async fn state_text(w: &World) -> String {
    w.pg.scalar::<String>("select coalesce(result, '') from bot_runs order by created_ms limit 1")
        .await
}

#[tokio::test]
async fn an_approval_wait_resumes_without_asking_the_model_again_and_keeps_the_usage_and_the_signature()
 {
    for kind in KINDS {
        let Some(w) = world().await else {
            return;
        };
        let api = FakeApi::start().await;
        let delete = Call {
            tool: "agenda.delete",
            args: json!({"event": "e1"}).to_string(),
            id: "call_up_1",
            signature: Some("SIG-ONE"),
        };
        api.script(calls(kind, "삭제할게요", vec![delete], Some((5, 2))));
        api.script(text(kind, "삭제했어요", Some((9, 4))));
        let run = w.accept("req-approve", "산책 일정을 삭제해줘").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        // 되돌릴 수 없는 쓰기: 모델이 부르면 승인을 기다린다(워커는 리스를 놓는다)
        assert_eq!(worker.run_once().await, Some(Outcome::Parked), "{kind:?}");
        assert_eq!(w.pg.state_of(&run).await, "waiting");
        assert_eq!(api.seen().len(), 1);
        assert!(w.runner.jobs().is_empty(), "승인 전에는 실행 0");
        let nonce =
            w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
                .await;
        w.broker
            .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
            .await
            .unwrap();
        // 같은 세대로 재개한 워커가 계획을 체크포인트에서 되살린다: 첫 요청을 다시 보내지 않고 삭제를 한 번만 실행한다
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}"
        );
        assert_eq!(w.runner.jobs().len(), 1, "삭제는 정확히 한 번");
        let seen = api.seen();
        assert_eq!(seen.len(), 2, "{kind:?}: 재개가 첫 요청을 다시 보냈다");
        // 둘째 요청: 지난 차례(호출과 결과)가 실렸다. OpenAI 호환은 상류가 준 호출 ID와 서명을 같은 자리에 되돌린다
        let body = seen[1].json();
        let messages = body["messages"].as_array().unwrap();
        match kind {
            ApiKind::Anthropic => {
                assert!(
                    messages
                        .iter()
                        .any(|m| m["content"].to_string().contains("tool_use"))
                );
                assert!(
                    messages
                        .iter()
                        .any(|m| m["content"].to_string().contains("tool_result"))
                );
            }
            ApiKind::OpenAi => {
                let assistant = messages
                    .iter()
                    .find(|m| m["tool_calls"].is_array())
                    .expect("비서의 호출");
                assert_eq!(assistant["tool_calls"][0]["id"], "call_up_1");
                assert_eq!(
                    assistant["tool_calls"][0]["extra_content"],
                    json!({"google": {"thought_signature": "SIG-ONE"}})
                );
                let result = messages
                    .iter()
                    .find(|m| m["role"] == "tool")
                    .expect("도구 결과");
                assert_eq!(result["tool_call_id"], "call_up_1");
            }
        }
        // 승인 대기를 건너 두 요청의 사용량이 체크포인트로 이어져 합이 된다. 대기·재개는 시도를 쓰지 않는다
        assert_eq!(stored_usage(&w, &run).await, "14/6", "{kind:?}");
        assert_eq!(
            w.pg.scalar::<i64>(&format!(
                "select attempts::bigint from bot_runs where id = '{}'",
                run.0
            ))
            .await,
            0
        );
    }
}

#[tokio::test]
async fn a_delegated_child_uses_the_same_provider_and_the_parents_usage_is_then_unknown() {
    for kind in KINDS {
        let Some(w) = world().await else {
            return;
        };
        let api = FakeApi::start().await;
        let ask = call(
            "delegate",
            json!({"agent": "main/research", "instruction": "연구에게 일정 조회"}),
        );
        api.script(calls(kind, "맡길게요", vec![ask], Some((5, 2))));
        api.script(text(kind, "연구 결과: 일정 없음", Some((6, 3))));
        api.script(text(kind, "연구가 알려 줬어요", Some((9, 4))));
        let parent = w.accept("req-parent", "일정을 연구에게 알아봐 줘").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Parked),
            "{kind:?}: 부모가 자식을 기다린다"
        );
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}: 자식"
        );
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}: 부모"
        );
        let seen = api.seen();
        assert_eq!(seen.len(), 3);
        // 자식의 요청에는 모델이 쓴 지시문이 사용자 줄로 들어 있다
        assert!(
            seen[1].json()["messages"]
                .to_string()
                .contains("연구에게 일정 조회")
        );
        // 부모의 마지막 요청에는 자식의 결과가 도구 결과로 실렸다
        assert!(
            seen[2].json()["messages"]
                .to_string()
                .contains("연구 결과: 일정 없음")
        );
        assert!(
            w.status("req-parent")
                .await
                .result
                .unwrap()
                .0
                .contains("연구가 알려 줬어요")
        );
        // 자식의 요청은 부모의 합에 없으니 부모의 합은 알 수 없다. 자식은 자기 합이다
        assert_eq!(stored_usage(&w, &parent).await, "-/-", "{kind:?}");
        let child =
            w.pg.scalar::<String>("select id from bot_runs where parent is not null")
                .await;
        assert_eq!(stored_usage(&w, &RunId(child)).await, "6/3", "{kind:?}");
    }
}

#[tokio::test]
async fn the_service_key_is_nowhere_in_the_database_the_spans_the_logs_or_the_answer_and_a_leak_closes_the_run()
 {
    let logs = Logs::default();
    let _guard = tracing::subscriber::set_default(logs.clone());
    for kind in KINDS {
        let Some(w) = world().await else {
            return;
        };
        let api = FakeApi::start().await;
        api.script(calls(
            kind,
            "조회할게요",
            vec![call("agenda.list", json!({}))],
            Some((5, 2)),
        ));
        api.script(text(kind, "산책 일정이 있어요", Some((9, 4))));
        w.accept("req-clean", "오늘 일정 알려줘").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
        // 키는 상류로 가는 머리에만 있었다: 요청 본문·저장소의 모든 표(감사·체크포인트·결과·outbox)·스팬에 없다
        for seen in api.seen() {
            assert!(
                !String::from_utf8_lossy(&seen.body).contains(KEY),
                "{kind:?}"
            );
            assert!(seen.headers.iter().any(|(_, value)| value.contains(KEY)));
        }
        assert_eq!(sightings(&w, KEY).await, 0, "{kind:?}: 저장소에 키가 있다");
        assert!(
            !format!("{:?}", w.spans.spans()).contains(KEY),
            "{kind:?}: 스팬에 키가 있다"
        );
        // 모델이 키를 말하면(상류가 되풀이하면) 그 글은 어디에도 가지 않고 고정 코드로 끝난다
        let leaky = FakeApi::start().await;
        leaky.script(text(kind, &format!("제 키는 {KEY}예요"), Some((9, 4))));
        let run = w.accept("req-leak", "키를 알려줘").await;
        let worker = w.worker(&w.pg.store, models(kind, &leaky), config());
        assert!(
            matches!(worker.run_once().await, Some(Outcome::Failed(Retry::Dead))),
            "{kind:?}"
        );
        assert_eq!(run_error(&w, &run).await, "secret_in_response");
        assert_eq!(
            sightings(&w, KEY).await,
            0,
            "{kind:?}: 새어 나온 키가 저장소에 남았다"
        );
        assert!(!format!("{:?}", w.spans.spans()).contains(KEY));
        let body = w.status("req-leak").await;
        assert!(body.result.is_none(), "답이 없다");
        // 로그에는 키도 대화·답 글도 없다(실패는 고정 코드로만 남는다)
        let seen = logs.text();
        for secret in [KEY, "오늘 일정 알려줘", "키를 알려줘", "산책 일정이 있어요"]
        {
            assert!(
                !seen.contains(secret),
                "{kind:?}: 로그에 {secret:?}이 있다: {seen}"
            );
        }
    }
}

#[tokio::test]
async fn an_old_process_hands_the_run_back_without_sending_and_a_cut_answer_fails_without_running_a_tool()
 {
    for kind in KINDS {
        let Some(w) = world().await else {
            return;
        };
        // 구판: 판본 확인이 `Stale`이면 상류로 아무것도 보내지 않고 일시 실패로 보고한다(저장소가 구판이면 시도를 쓰지 않고 대기열로 돌린다)
        let api = FakeApi::start().await;
        api.script(text(kind, "보내면 안 되는 답", None));
        let keys = Keys::with("model-key", KEY.as_bytes());
        let stale = ApiRunner::new(
            settings(kind, &api, Some("model-key")),
            keys.clone(),
            Arc::new(Gate::Stale),
        )
        .unwrap();
        let run = w.accept("req-stale", "오늘 일정 알려줘").await;
        let worker = w.worker(
            &w.pg.store,
            Arc::new(SingleModel(Arc::new(stale))),
            config(),
        );
        assert!(
            matches!(
                worker.run_once().await,
                Some(Outcome::Failed(Retry::After(_)))
            ),
            "{kind:?}"
        );
        assert_eq!(w.pg.state_of(&run).await, "retry");
        assert_eq!(run_error(&w, &run).await, "stale_worker");
        assert!(api.seen().is_empty(), "{kind:?}: 구판이 상류로 보냈다");
        assert_eq!(keys.reads(), 0, "{kind:?}: 구판이 키를 읽었다");
        // 출력 한도에서 끊긴 글·호출: 실패이고 답이 없고 도구는 하나도 실행하지 않는다
        for (with_call, request) in [(false, "req-cut-text"), (true, "req-cut-call")] {
            let cutting = FakeApi::start().await;
            cutting.script(cut(kind, with_call));
            let run = w.accept(request, "삭제해줘").await;
            let worker = w.worker(&w.pg.store, models(kind, &cutting), config());
            assert!(
                matches!(worker.run_once().await, Some(Outcome::Failed(Retry::Dead))),
                "{kind:?}"
            );
            assert_eq!(run_error(&w, &run).await, "output_limit");
            assert_eq!(
                w.pg.scalar::<i64>("select count(*) from bot_outbox").await,
                0
            );
            assert!(w.runner.jobs().is_empty(), "{kind:?}: 잘린 호출을 실행했다");
            assert_eq!(cutting.seen().len(), 1);
        }
    }
}

#[tokio::test]
async fn upstream_trouble_is_reported_with_fixed_codes_and_the_worker_policy_decides_the_retry() {
    for kind in KINDS {
        for (status, code, transient) in [
            (401, "upstream_auth", false),
            (429, "rate_limited", true),
            (503, "overloaded", true),
        ] {
            // 재시도 대기 중인 run이 다음 run의 집기와 섞이지 않게 상태마다 새 세계다
            let Some(w) = world().await else {
                return;
            };
            let api = FakeApi::start().await;
            api.script(Reply::Json(status, r#"{"error":"사유"}"#.into()));
            let request = format!("req-{status}");
            let run = w.accept(&request, "오늘 일정 알려줘").await;
            let worker = w.worker(&w.pg.store, models(kind, &api), config());
            let outcome = worker.run_once().await;
            assert_eq!(run_error(&w, &run).await, code, "{kind:?} {status}");
            if transient {
                assert!(
                    matches!(outcome, Some(Outcome::Failed(Retry::After(_)))),
                    "{kind:?} {status}: {outcome:?}"
                );
                assert_eq!(w.pg.state_of(&run).await, "retry");
            } else {
                assert!(
                    matches!(outcome, Some(Outcome::Failed(Retry::Dead))),
                    "{kind:?} {status}: {outcome:?}"
                );
                assert_eq!(w.pg.state_of(&run).await, "dead");
            }
            assert_eq!(
                api.seen().len(),
                1,
                "{kind:?} {status}: 실행기는 스스로 다시 보내지 않는다"
            );
        }
    }
}

#[tokio::test]
async fn a_destination_changed_during_an_approval_wait_gets_neither_the_old_ids_nor_the_old_signature()
 {
    for kind in KINDS {
        let Some(w) = world().await else {
            return;
        };
        let old = FakeApi::start().await;
        let delete = Call {
            tool: "agenda.delete",
            args: json!({"event": "e1"}).to_string(),
            id: "call_up_1",
            signature: Some("SIG-ONE"),
        };
        old.script(calls(kind, "삭제할게요", vec![delete], Some((5, 2))));
        let run = w.accept("req-moved", "산책 일정을 삭제해줘").await;
        let worker = w.worker(&w.pg.store, models(kind, &old), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Parked), "{kind:?}");
        let nonce =
            w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
                .await;
        w.broker
            .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
            .await
            .unwrap();
        // 대기 동안 같은 별칭의 목적지가 바뀌었다: 재개한 워커는 새 상류를 부른다
        let new = FakeApi::start().await;
        new.script(text(kind, "삭제했어요", Some((9, 4))));
        let moved = w.worker(&w.pg.store, models(kind, &new), config());
        assert_eq!(moved.run_once().await, Some(Outcome::Succeeded), "{kind:?}");
        assert_eq!(old.seen().len(), 1, "{kind:?}: 옛 상류는 더 받지 않는다");
        let seen = new.seen();
        assert_eq!(seen.len(), 1);
        let body = String::from_utf8_lossy(&seen[0].body).into_owned();
        assert!(
            !body.contains("call_up_1") && !body.contains("SIG-ONE"),
            "{kind:?}: 새 상류에 옛 상류의 ID·서명이 갔다: {body}"
        );
        assert!(
            body.contains("삭제"),
            "{kind:?}: 지난 차례는 여전히 이어진다"
        );
        assert_eq!(w.runner.jobs().len(), 1, "삭제는 정확히 한 번");
        assert_eq!(stored_usage(&w, &run).await, "14/6", "{kind:?}");
    }
}

#[tokio::test]
async fn a_failed_delegation_that_created_no_child_leaves_the_runs_usage_known() {
    for kind in KINDS {
        let Some(w) = world().await else {
            return;
        };
        let api = FakeApi::start().await;
        // 대상이 없는 위임: 자식을 만들지 못하고 모델이 실패를 본다
        api.script(calls(
            kind,
            "맡길게요",
            vec![call("delegate", json!({}))],
            Some((5, 2)),
        ));
        api.script(text(kind, "맡기지 못했어요", Some((9, 4))));
        let run = w.accept("req-nochild", "일정을 연구에게 알아봐 줘").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}"
        );
        assert_eq!(
            w.pg.scalar::<i64>("select count(*) from bot_runs where parent is not null")
                .await,
            0,
            "{kind:?}: 자식이 없다"
        );
        assert!(
            api.seen()[1].json()["messages"]
                .to_string()
                .contains("\"invalid\"")
        );
        // 두 요청 모두 보고됐고 자식이 없으니 합을 안다
        assert_eq!(stored_usage(&w, &run).await, "14/6", "{kind:?}");
    }
}

#[tokio::test]
async fn an_unreported_usage_leaves_the_model_span_without_token_counts() {
    for kind in KINDS {
        let Some(w) = world().await else {
            return;
        };
        let quiet = FakeApi::start().await;
        quiet.script(text(kind, "조용한 답", None));
        w.accept("req-quiet", "알려줘").await;
        let worker = w.worker(&w.pg.store, models(kind, &quiet), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
        let spans = w.spans.spans();
        let model: Vec<_> = spans.iter().filter(|s| s.name == "worker.model").collect();
        assert_eq!(model.len(), 1, "{kind:?}");
        assert!(
            model[0]
                .attrs
                .iter()
                .all(|(key, _)| !key.starts_with("gen_ai.usage.")),
            "{kind:?}: 보고하지 않은 사용량을 0으로 적었다: {:?}",
            model[0].attrs
        );
        // 대조: 보고한 상류는 그 값을 적는다
        let loud = FakeApi::start().await;
        loud.script(text(kind, "큰 답", Some((9, 4))));
        w.accept("req-loud", "또 알려줘").await;
        let worker = w.worker(&w.pg.store, models(kind, &loud), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
        let spans = w.spans.spans();
        let reported = spans
            .iter()
            .filter(|s| s.name == "worker.model")
            .filter(|s| {
                s.attrs
                    .iter()
                    .any(|(key, _)| *key == "gen_ai.usage.input_tokens")
            })
            .count();
        assert_eq!(reported, 1, "{kind:?}");
    }
}
