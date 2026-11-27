//! End-to-end tests for the direct API runner (`RunnerKind::Step`) over PostgreSQL, BrokerService, and loopback FakeApi across both Anthropic and OpenAI dialects.

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

async fn sightings(w: &World, needle: &str) -> i64 {
    let tables: Vec<String> = sqlx::query_scalar(
        "select tablename::text from pg_tables where schemaname = current_schema()",
    )
    .fetch_all(w.pg.store.pool())
    .await
    .unwrap();
    assert!(tables.len() > 10, "expected schema tables: {tables:?}");
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
        api.script(text(kind, "No events scheduled", Some((9, 4))));
        let run = w.accept("req-final", "Show today's agenda").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}"
        );
        let state = w.status("req-final").await;
        assert!(state.result.unwrap().0.contains("No events scheduled"));
        assert_eq!(stored_usage(&w, &run).await, "9/4", "{kind:?}");
        assert_eq!(api.seen().len(), 1);
        assert!(w.runner.jobs().is_empty());

        let quiet = FakeApi::start().await;
        quiet.script(text(kind, "Quiet answer", None));
        let run = w.accept("req-quiet", "Check again").await;
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
            "Checking agenda",
            vec![call("agenda.list", json!({}))],
            Some((5, 2)),
        ));
        api.script(text(kind, "You have a Walk event", Some((9, 4))));
        let run = w.accept("req-tool", "Show today's agenda").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}"
        );
        assert_eq!(w.runner.jobs().len(), 1);
        let seen = api.seen();
        assert_eq!(seen.len(), 2);
        // Untrusted tool output belongs only in conversation messages, never in the system slot.
        let second = seen[1].json();
        assert!(second["messages"].to_string().contains("Walk"), "{kind:?}");
        let system = match kind {
            ApiKind::Anthropic => second["system"].to_string(),
            ApiKind::OpenAi => second["messages"][0].to_string(),
        };
        assert!(
            !system.contains("Walk"),
            "{kind:?}: tool output leaked into system prompt"
        );
        assert!(state_text(&w).await.contains("You have a Walk event"));
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
        api.script(calls(kind, "Deleting event", vec![delete], Some((5, 2))));
        api.script(text(kind, "Deleted", Some((9, 4))));
        let run = w.accept("req-approve", "Delete the walk event").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Parked), "{kind:?}");
        assert_eq!(w.pg.state_of(&run).await, "waiting");
        assert_eq!(api.seen().len(), 1);
        assert!(w.runner.jobs().is_empty());
        let nonce =
            w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
                .await;
        w.broker
            .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
            .await
            .unwrap();
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}"
        );
        assert_eq!(w.runner.jobs().len(), 1);
        let seen = api.seen();
        assert_eq!(seen.len(), 2, "{kind:?}: resume resent the first turn");
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
                    .expect("assistant tool call");
                assert_eq!(assistant["tool_calls"][0]["id"], "call_up_1");
                assert_eq!(
                    assistant["tool_calls"][0]["extra_content"],
                    json!({"google": {"thought_signature": "SIG-ONE"}})
                );
                let result = messages
                    .iter()
                    .find(|m| m["role"] == "tool")
                    .expect("tool result");
                assert_eq!(result["tool_call_id"], "call_up_1");
            }
        }
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
            json!({"agent": "main/research", "instruction": "Ask research to check the agenda"}),
        );
        api.script(calls(kind, "Delegating", vec![ask], Some((5, 2))));
        api.script(text(kind, "Research result: no events", Some((6, 3))));
        api.script(text(kind, "Research replied", Some((9, 4))));
        let parent = w.accept("req-parent", "Ask research about my agenda").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Parked),
            "{kind:?}: parent waits for child"
        );
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}: child"
        );
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}: parent"
        );
        let seen = api.seen();
        assert_eq!(seen.len(), 3);
        assert!(
            seen[1].json()["messages"]
                .to_string()
                .contains("Ask research to check the agenda")
        );
        assert!(
            seen[2].json()["messages"]
                .to_string()
                .contains("Research result: no events")
        );
        assert!(
            w.status("req-parent")
                .await
                .result
                .unwrap()
                .0
                .contains("Research replied")
        );
        // Child tokens are tracked on the child run, leaving parent total usage unknown.
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
            "Checking agenda",
            vec![call("agenda.list", json!({}))],
            Some((5, 2)),
        ));
        api.script(text(kind, "You have a Walk event", Some((9, 4))));
        w.accept("req-clean", "Show today's agenda").await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Succeeded));
        for seen in api.seen() {
            assert!(
                !String::from_utf8_lossy(&seen.body).contains(KEY),
                "{kind:?}"
            );
            assert!(seen.headers.iter().any(|(_, value)| value.contains(KEY)));
        }
        assert_eq!(sightings(&w, KEY).await, 0, "{kind:?}: key found in DB");
        assert!(
            !format!("{:?}", w.spans.spans()).contains(KEY),
            "{kind:?}: key found in spans"
        );

        let leaky = FakeApi::start().await;
        leaky.script(text(kind, &format!("My key is {KEY}"), Some((9, 4))));
        let run = w.accept("req-leak", "Tell me the key").await;
        let worker = w.worker(&w.pg.store, models(kind, &leaky), config());
        assert!(
            matches!(worker.run_once().await, Some(Outcome::Failed(Retry::Dead))),
            "{kind:?}"
        );
        assert_eq!(run_error(&w, &run).await, "secret_in_response");
        assert_eq!(
            sightings(&w, KEY).await,
            0,
            "{kind:?}: leaked key persisted in DB"
        );
        assert!(!format!("{:?}", w.spans.spans()).contains(KEY));
        let body = w.status("req-leak").await;
        assert!(body.result.is_none());
        let seen = logs.text();
        for secret in [
            KEY,
            "Show today's agenda",
            "Tell me the key",
            "You have a Walk event",
        ] {
            assert!(
                !seen.contains(secret),
                "{kind:?}: {secret:?} leaked into logs: {seen}"
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
        let api = FakeApi::start().await;
        api.script(text(kind, "Must not be sent", None));
        let keys = Keys::with("model-key", KEY.as_bytes());
        let stale = ApiRunner::new(
            settings(kind, &api, Some("model-key")),
            keys.clone(),
            Arc::new(Gate::Stale),
        )
        .unwrap();
        let run = w.accept("req-stale", "Show today's agenda").await;
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
        assert!(api.seen().is_empty(), "{kind:?}: stale worker sent request");
        assert_eq!(keys.reads(), 0, "{kind:?}: stale worker read secret");

        for (with_call, request) in [(false, "req-cut-text"), (true, "req-cut-call")] {
            let cutting = FakeApi::start().await;
            cutting.script(cut(kind, with_call));
            let run = w.accept(request, "Delete event").await;
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
            assert!(
                w.runner.jobs().is_empty(),
                "{kind:?}: truncated tool call executed"
            );
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
            // Fresh schema per status so a retry-queued run does not interfere with the next case.
            let Some(w) = world().await else {
                return;
            };
            let api = FakeApi::start().await;
            api.script(Reply::Json(status, r#"{"error":"reason"}"#.into()));
            let request = format!("req-{status}");
            let run = w.accept(&request, "Show today's agenda").await;
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
                "{kind:?} {status}: runner must not retry"
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
        old.script(calls(kind, "Deleting event", vec![delete], Some((5, 2))));
        let run = w.accept("req-moved", "Delete the walk event").await;
        let worker = w.worker(&w.pg.store, models(kind, &old), config());
        assert_eq!(worker.run_once().await, Some(Outcome::Parked), "{kind:?}");
        let nonce =
            w.pg.scalar::<String>("select nonce from bot_approvals where state = 'pending'")
                .await;
        w.broker
            .decide(ApprovalId(nonce), PrincipalId(OWNER.into()), true)
            .await
            .unwrap();
        let new = FakeApi::start().await;
        new.script(text(kind, "Deleted", Some((9, 4))));
        let moved = w.worker(&w.pg.store, models(kind, &new), config());
        assert_eq!(moved.run_once().await, Some(Outcome::Succeeded), "{kind:?}");
        assert_eq!(old.seen().len(), 1);
        let seen = new.seen();
        assert_eq!(seen.len(), 1);
        let body = String::from_utf8_lossy(&seen[0].body).into_owned();
        assert!(
            !body.contains("call_up_1") && !body.contains("SIG-ONE"),
            "{kind:?}: old upstream call ID or signature leaked to new endpoint: {body}"
        );
        assert!(
            body.contains("Deleting"),
            "{kind:?}: prior turn text must still carry over"
        );
        assert_eq!(w.runner.jobs().len(), 1);
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
        api.script(calls(
            kind,
            "Delegating",
            vec![call("delegate", json!({}))],
            Some((5, 2)),
        ));
        api.script(text(kind, "Could not delegate", Some((9, 4))));
        let run = w
            .accept("req-nochild", "Ask research about my agenda")
            .await;
        let worker = w.worker(&w.pg.store, models(kind, &api), config());
        assert_eq!(
            worker.run_once().await,
            Some(Outcome::Succeeded),
            "{kind:?}"
        );
        assert_eq!(
            w.pg.scalar::<i64>("select count(*) from bot_runs where parent is not null")
                .await,
            0
        );
        assert!(
            api.seen()[1].json()["messages"]
                .to_string()
                .contains("\"invalid\"")
        );
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
        quiet.script(text(kind, "Quiet answer", None));
        w.accept("req-quiet", "Tell me").await;
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
            "{kind:?}: unreported usage recorded on span: {:?}",
            model[0].attrs
        );

        let loud = FakeApi::start().await;
        loud.script(text(kind, "Loud answer", Some((9, 4))));
        w.accept("req-loud", "Tell me again").await;
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
