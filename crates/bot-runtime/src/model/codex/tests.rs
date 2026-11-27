//! Unit tests for Codex CLI argument assembly, stream parsing, token confinement, and rollout session recovery.

use super::*;
use crate::model::{
    FakeSandbox, Limits, Message, ModelStep, PortFuture, Role, Script, SessionError, SessionPort,
    sandbox::files_holding,
    session::{Pending, ToolOutput},
};
use bot_core::{
    cli::{CliResponse, CliResponseUse, codex_response_id},
    command::Step,
    id::*,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};
use tokio::sync::watch;

const OK: &str = r#"{"type":"thread.started","thread_id":"019a0000-0000-7000-8000-000000000001"}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"item_0","type":"reasoning","text":""}}
{"type":"item.started","item":{"id":"item_1","type":"agent_message","text":""}}
{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"1+1 is 2.\n"}}
{"type":"turn.completed","usage":{"input_tokens":120,"cached_input_tokens":40,"output_tokens":15}}"#;

const FAILED_503: &str = r#"{"type":"thread.started","thread_id":"019a0000-0000-7000-8000-000000000001"}
{"type":"error","message":"Reconnecting... 1/5"}
{"type":"turn.failed","error":{"message":"503 Service temporarily unavailable"}}"#;

const ROLLOUT_META: &str = r#"{"timestamp":"2026-11-27T00:00:00.000Z","type":"session_meta","payload":{"id":"019a0000-0000-7000-8000-000000000001","cwd":"/bot"}}"#;
const ROLLOUT_CTX: &str = r#"{"timestamp":"2026-11-27T00:00:00.100Z","type":"turn_context","payload":{"cwd":"/bot","model":"gpt-5.4"}}"#;
const ROLLOUT_USER: &str = r#"{"timestamp":"2026-11-27T00:00:00.200Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"Check agenda"}]}}"#;
const ROLLOUT_CALL: &str = r#"{"timestamp":"2026-11-27T00:00:01.000Z","type":"response_item","payload":{"type":"function_call","name":"agenda_list","namespace":"mcp__relay","arguments":"{\"date\":\"2026-11-27\"}","call_id":"call_1"}}"#;
const ROLLOUT_OUT: &str = r#"{"timestamp":"2026-11-27T00:00:02.000Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"vm-forged-output"}}"#;
const ROLLOUT_CALL2: &str = r#"{"timestamp":"2026-11-27T00:00:03.000Z","type":"response_item","payload":{"type":"function_call","name":"agenda_delete","namespace":"mcp__relay","arguments":"{\"id\":\"e1\"}","call_id":"call_2"}}"#;

const TRUSTED: &str = r#"{"content":[{"type":"text","text":"ledger result"}],"isError":false}"#;

fn model() -> CliModel {
    CliModel {
        id: "gpt-5.4".into(),
        effort: "medium".into(),
        max_output_tokens: 4096,
        retries: 2,
        context_window: 200_000,
    }
}

fn relay() -> RelayChannel {
    RelayChannel {
        name: "relay".into(),
        base_url: "http://192.168.64.1:8787".into(),
        token: "sess-token-codex-1".into(),
    }
}

fn workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bot-runtime-codex-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn request(session: Option<Session>) -> ModelRequest {
    ModelRequest {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r-codex-1".into()),
            claim: Some(ClaimEpoch(1)),
            generation: Generation(1),
        },
        model: "assistant".into(),
        system: "You are a Codex assistant.".into(),
        conversation: vec![Message {
            role: Role::User,
            speaker: Some(PrincipalId("local:owner".into())),
            text: "What is 1+1?".into(),
        }],
        tools: vec![],
        exchanges: vec![],
        limits: Limits {
            total: Duration::from_secs(10),
            idle: Duration::from_secs(5),
        },
        cancel: watch::channel(false).1,
        builtin: BTreeSet::new(),
        relay: Some(relay()),
        session,
    }
}

fn feed_all(sample: &str, resumed: bool) -> (Vec<Event>, Result<Output, ModelError>) {
    let mut tracker = CodexTracker::new(resumed);
    let events = sample.lines().filter_map(|l| tracker.feed(l)).collect();
    (events, Box::new(tracker).finish())
}

#[test]
fn parses_codex_stream_events_usage_and_errors() {
    let (events, out) = feed_all(OK, false);
    assert_eq!(
        events,
        [
            Event::Thinking,
            Event::Thinking,
            Event::Writing,
            Event::Writing
        ]
    );
    let out = out.unwrap();
    assert_eq!(out.text, "1+1 is 2.");
    assert_eq!(
        out.usage,
        Usage {
            input_tokens: 120,
            output_tokens: 15
        }
    );
    assert_eq!(
        out.outcome.usage,
        Some(TokenUsage {
            prompt: 120,
            completion: 15
        })
    );
    // Resumed invocations suppress partial token usage reporting.
    assert_eq!(feed_all(OK, true).1.unwrap().outcome.usage, None);

    let (events, err) = feed_all(FAILED_503, false);
    assert_eq!(events, [Event::Retrying]);
    assert_eq!(
        err,
        Err(ModelError::Failed {
            code: "overloaded",
            transient: true
        })
    );
    assert_eq!(
        feed_all(r#"{"type":"turn.completed","usage":{}}"#, false).1,
        Err(ModelError::Failed {
            code: "empty_answer",
            transient: false
        })
    );
    assert_eq!(
        feed_all(r#"{"type":"thread.started"}"#, false).1,
        Err(ModelError::Failed {
            code: "no_result",
            transient: false
        })
    );
}

#[test]
fn args_configure_relay_provider_disable_builtins_and_never_contain_the_token() {
    let fresh = CodexRunner::args(&model(), &BTreeSet::new(), &relay(), &Mode::Fresh).unwrap();
    assert_eq!(fresh[0], "exec");
    assert_eq!(fresh.last().map(String::as_str), Some("-"));
    assert!(!fresh.contains(&"--ephemeral".to_owned()));
    assert!(!fresh.contains(&"resume".to_owned()));
    for want in [
        "model_provider=\"relay\"",
        "model_providers.relay.name=\"relay\"",
        "model_providers.relay.base_url=\"http://192.168.64.1:8787/v1\"",
        "model_providers.relay.env_key=\"RELAY_TOKEN\"",
        "model_providers.relay.wire_api=\"responses\"",
        "mcp_servers.relay.url=\"http://192.168.64.1:8787/mcp\"",
        "mcp_servers.relay.bearer_token_env_var=\"RELAY_TOKEN\"",
        "mcp_servers.relay.enabled=true",
        "web_search=\"disabled\"",
        "model_reasoning_effort=\"medium\"",
    ] {
        assert!(
            fresh.windows(2).any(|w| w == ["-c", want]),
            "missing -c {want} in {fresh:?}"
        );
    }
    for feature in DISABLED_FEATURES {
        assert!(
            fresh.windows(2).any(|w| w == ["--disable", feature]),
            "missing --disable {feature}"
        );
    }
    assert!(fresh.iter().all(|a| !a.contains("sess-token-codex-1")));

    let eph = CodexRunner::args(&model(), &BTreeSet::new(), &relay(), &Mode::Ephemeral).unwrap();
    assert!(eph.contains(&"--ephemeral".to_owned()));

    let res = CodexRunner::args(
        &model(),
        &BTreeSet::new(),
        &relay(),
        &Mode::Resume("019a-thread".into()),
    )
    .unwrap();
    assert_eq!(&res[..3], ["exec", "resume", "--all"]);
    assert_eq!(
        &res[res.len() - 2..],
        ["019a-thread", prompt::RESUME_INSTRUCTION]
    );
    assert!(!fresh.contains(&"--all".to_owned()));
    assert!(!eph.contains(&"--all".to_owned()));

    let err = CodexRunner::args(
        &model(),
        &BTreeSet::from([Builtin::Read]),
        &relay(),
        &Mode::Fresh,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ModelError::Failed {
            code: "unsupported_builtin",
            transient: false
        }
    );
}

#[tokio::test]
async fn step_keeps_session_token_off_disk_and_rejects_builtin_tools() {
    let ws = workspace("spec");
    let found = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let sink = found.clone();
    let sandbox = FakeSandbox::new([Script::output(OK)]).inspect(move |spec| {
        let input = &spec.readonly_mounts[0].host;
        let work = &spec.scratch.as_ref().unwrap().host;
        *sink.lock().unwrap() = files_holding(&[input, work], "sess-token-codex-1");
    });
    let config = CliConfig::new(&ws).with_model("assistant", model());
    let runner = CodexRunner::new(Arc::new(sandbox.clone()), config);
    assert_eq!(runner.kind(), RunnerKind::Loop);
    assert_eq!(
        runner.meta("assistant"),
        Some(ModelMeta {
            provider: "openai",
            model: "gpt-5.4".into(),
        })
    );

    let step = runner.step(request(None)).await.unwrap();
    assert!(matches!(step, ModelStep::Final { ref text, .. } if text == "1+1 is 2."));
    assert!(found.lock().unwrap().is_empty());

    let spec = sandbox.specs().remove(0);
    assert_eq!(spec.program, "codex");
    assert_eq!(spec.cwd, "/bot");
    assert!(spec.args.iter().all(|a| !a.contains("sess-token-codex-1")));
    let carrying: Vec<&str> = spec
        .env
        .iter()
        .filter(|(_, v)| v.contains("sess-token-codex-1"))
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(carrying, ["RELAY_TOKEN"]);
    assert!(spec.stdin.is_some());

    let mut bad = request(None);
    bad.builtin.insert(Builtin::Read);
    assert_eq!(
        runner.step(bad).await.unwrap_err(),
        ModelError::Failed {
            code: "unsupported_builtin",
            transient: false
        }
    );
    assert_eq!(std::fs::read_dir(&ws).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&ws);
}

#[derive(Default)]
struct FakePort {
    steps: Vec<Step>,
    commits: Mutex<Vec<Vec<Step>>>,
    responses: Mutex<BTreeMap<String, CliResponse>>,
    unfinished: Mutex<BTreeSet<(u64, u32)>>,
    settle: Mutex<VecDeque<Result<Slot, SessionError>>>,
    recalled: Mutex<Vec<(u64, u32, String)>>,
    commit_error: Mutex<Option<SessionError>>,
}

impl FakePort {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps,
            ..Self::default()
        })
    }

    fn confirm(&self, response: CliResponse) {
        self.responses
            .lock()
            .unwrap()
            .insert(response.id.clone(), response);
    }

    fn commits(&self) -> Vec<Vec<Step>> {
        self.commits.lock().unwrap().clone()
    }
}

impl SessionPort for FakePort {
    fn steps(&self) -> &[Step] {
        &self.steps
    }

    fn commit(&self, steps: Vec<Step>) -> PortFuture<'_, ()> {
        let err = *self.commit_error.lock().unwrap();
        if err.is_none() {
            self.commits.lock().unwrap().push(steps);
        }
        Box::pin(async move { err.map_or(Ok(()), Err) })
    }

    fn response(&self, message_id: &str) -> PortFuture<'_, Option<CliResponse>> {
        let found = self.responses.lock().unwrap().get(message_id).cloned();
        Box::pin(async move { Ok(found) })
    }

    fn recall<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, Option<ToolOutput>> {
        self.recalled
            .lock()
            .unwrap()
            .push((slot.turn, slot.pos, slot.tool_use.id.clone()));
        let finished = !self
            .unfinished
            .lock()
            .unwrap()
            .contains(&(slot.turn, slot.pos));
        Box::pin(async move { Ok(finished.then(|| ToolOutput::from_payload(TRUSTED))) })
    }

    fn ended<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, bool> {
        let finished = !self
            .unfinished
            .lock()
            .unwrap()
            .contains(&(slot.turn, slot.pos));
        Box::pin(async move { Ok(finished) })
    }

    fn settle<'a>(&'a self, _slot: &'a Pending) -> PortFuture<'a, Slot> {
        let next = self.settle.lock().unwrap().pop_front();
        Box::pin(async move { next.unwrap_or(Ok(Slot::Result(ToolOutput::notice("aborted")))) })
    }
}

fn confirm_call1() -> CliResponse {
    CliResponse {
        id: codex_response_id("call_1"),
        uses: vec![CliResponseUse {
            id: "call_1".into(),
            name: "mcp__relay__agenda_list".into(),
            input: json!({"date": "2026-11-27"}),
        }],
    }
}

fn confirm_call2() -> CliResponse {
    CliResponse {
        id: codex_response_id("call_2"),
        uses: vec![CliResponseUse {
            id: "call_2".into(),
            name: "mcp__relay__agenda_delete".into(),
            input: json!({"id": "e1"}),
        }],
    }
}

fn write_rollout(spec: &ModelSpec, lines: &[&str]) {
    let path = spec
        .scratch
        .as_ref()
        .unwrap()
        .host
        .join("codex/sessions/2026/11/27/rollout-2026-11-27T00-00-00-019a0000-0000-7000-8000-000000000001.jsonl");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
}

#[tokio::test]
async fn fresh_session_discovers_rollout_confirms_via_broker_and_commits_batch() {
    let ws = workspace("fresh-session");
    let port = FakePort::new(vec![]);
    port.confirm(confirm_call1());
    let sandbox =
        FakeSandbox::new([Script::output(OK).slow(Duration::from_millis(40))]).inspect(|spec| {
            write_rollout(
                spec,
                &[ROLLOUT_META, ROLLOUT_CTX, ROLLOUT_USER, ROLLOUT_CALL],
            );
        });
    let runner = CodexRunner::new(
        Arc::new(sandbox.clone()),
        CliConfig::new(&ws).with_model("assistant", model()),
    );
    let out = runner
        .step(request(Some(Session(port.clone()))))
        .await
        .unwrap();
    assert!(matches!(out, ModelStep::Final { ref text, .. } if text == "1+1 is 2."));
    let commits = port.commits();
    assert_eq!(commits.len(), 1);
    let keys: Vec<&str> = commits[0].iter().map(|s| s.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "cli-rec-0",
            "cli-rec-1",
            "cli-rec-2",
            "cli-rec-3",
            "cli-use-call_1",
            "cli-group-3",
        ]
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn mismatch_and_symlink_rollouts_stop_the_cli_without_committing() {
    let ws = workspace("unsafe");
    let port = FakePort::new(vec![]);
    let mut wrong = confirm_call1();
    wrong.uses[0].input = json!({"date": "1999-01-01"});
    port.confirm(wrong);
    let sandbox = FakeSandbox::new([Script::output(OK).slow(Duration::from_millis(40)).hangs()])
        .inspect(|spec| {
            write_rollout(
                spec,
                &[ROLLOUT_META, ROLLOUT_CTX, ROLLOUT_USER, ROLLOUT_CALL],
            );
        });
    let runner = CodexRunner::new(
        Arc::new(sandbox.clone()),
        CliConfig::new(&ws).with_model("assistant", model()),
    );
    assert_eq!(
        runner
            .step(request(Some(Session(port.clone()))))
            .await
            .unwrap_err(),
        ModelError::Failed {
            code: "session_mismatch",
            transient: false
        }
    );
    assert!(port.commits().is_empty());
    assert_eq!(sandbox.killed(), 1);

    let outside = std::env::temp_dir().join(format!("bot-codex-outside-{}", std::process::id()));
    std::fs::write(&outside, format!("{ROLLOUT_META}\n")).unwrap();
    let target = outside.clone();
    let port = FakePort::new(vec![]);
    port.confirm(confirm_call1());
    let sandbox = FakeSandbox::new([Script::output(OK).slow(Duration::from_millis(40)).hangs()])
        .inspect(move |spec| {
            let dir = spec
                .scratch
                .as_ref()
                .unwrap()
                .host
                .join("codex/sessions/2026/11/27");
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(&target, dir.join("rollout-evil.jsonl")).unwrap();
        });
    let runner = CodexRunner::new(
        Arc::new(sandbox.clone()),
        CliConfig::new(&ws).with_model("assistant", model()),
    );
    assert_eq!(
        runner
            .step(request(Some(Session(port.clone()))))
            .await
            .unwrap_err(),
        ModelError::Failed {
            code: "session_unsafe",
            transient: false
        }
    );
    assert_eq!(sandbox.killed(), 1);
    let _ = std::fs::remove_file(outside);
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn resume_overwrites_vm_results_from_ledger_settles_pending_and_resumes_thread() {
    let ws = workspace("resume");
    let mut journal = Journal::new(0, Default::default(), "mcp__relay__".into());
    for line in [
        ROLLOUT_META,
        ROLLOUT_CTX,
        ROLLOUT_USER,
        ROLLOUT_CALL,
        ROLLOUT_OUT,
        ROLLOUT_CALL2,
    ] {
        journal.feed(line);
    }
    let plan1 = journal.plan(&confirm_call1()).unwrap();
    journal.applied(&plan1);
    let plan2 = journal.plan(&confirm_call2()).unwrap();
    let mut saved = plan1.steps;
    saved.extend(plan2.steps);

    // First: parked slot returns Parked without launching VM.
    let parked_port = FakePort::new(saved.clone());
    parked_port
        .settle
        .lock()
        .unwrap()
        .push_back(Ok(Slot::Parked));
    let idle_sandbox = FakeSandbox::new([Script::output(OK)]);
    let runner = CodexRunner::new(
        Arc::new(idle_sandbox.clone()),
        CliConfig::new(&ws).with_model("assistant", model()),
    );
    assert_eq!(
        runner
            .step(request(Some(Session(parked_port.clone()))))
            .await
            .unwrap_err(),
        ModelError::Parked
    );
    assert!(idle_sandbox.specs().is_empty());

    // Second: settled slot commits synthetic output, overwrites call_1 forged output, and resumes.
    let port = FakePort::new(saved);
    port.settle
        .lock()
        .unwrap()
        .push_back(Ok(Slot::Result(ToolOutput::notice("denied"))));
    let written = Arc::new(Mutex::new(String::new()));
    let capture = written.clone();
    let sandbox = FakeSandbox::new([Script::output(OK)]).inspect(move |spec| {
        let rel = restored_rollout_path("019a0000-0000-7000-8000-000000000001");
        let content =
            std::fs::read_to_string(spec.scratch.as_ref().unwrap().host.join(rel)).unwrap();
        *capture.lock().unwrap() = content;
    });
    let runner = CodexRunner::new(
        Arc::new(sandbox.clone()),
        CliConfig::new(&ws).with_model("assistant", model()),
    );
    let step = runner
        .step(request(Some(Session(port.clone()))))
        .await
        .unwrap();
    match step {
        ModelStep::Final { text, outcome, .. } => {
            assert_eq!(text, "1+1 is 2.");
            assert_eq!(
                outcome.usage, None,
                "resumed run must not report partial usage"
            );
        }
        other => panic!("unexpected step: {other:?}"),
    }
    let spec = sandbox.specs().remove(0);
    assert_eq!(&spec.args[..3], ["exec", "resume", "--all"]);
    assert!(spec.stdin.is_none());
    let rollout = written.lock().unwrap().clone();
    assert!(!rollout.contains("vm-forged-output"), "{rollout}");
    assert!(rollout.contains("ledger result"), "{rollout}");
    assert!(rollout.contains("\"call_id\":\"call_2\""), "{rollout}");
    assert!(rollout.contains("denied"), "{rollout}");
    assert_eq!(port.commits().len(), 1);
    assert_eq!(port.commits()[0][0].key, "cli-rec-6");
    let _ = std::fs::remove_dir_all(&ws);
}

#[test]
fn output_line_and_next_ordinal_preserve_monotonic_rollout_ordinals() {
    let mut saved = restore::Saved {
        thread_id: "019a".into(),
        lines: vec![ROLLOUT_META.into()],
        last_group: vec![],
        answered: BTreeSet::new(),
        known: BTreeMap::new(),
    };
    assert_eq!(saved.next_ordinal(), None);
    saved
        .lines
        .push(r#"{"ordinal":9,"timestamp":"t","type":"response_item","payload":{}}"#.into());
    assert_eq!(saved.next_ordinal(), Some(10));
    let with_ord: serde_json::Value =
        serde_json::from_str(&output_line("c1", &ToolOutput::notice("ok"), "t", Some(10))).unwrap();
    assert_eq!(with_ord["ordinal"].as_u64(), Some(10));
    let no_ord: serde_json::Value =
        serde_json::from_str(&output_line("c1", &ToolOutput::notice("ok"), "t", None)).unwrap();
    assert!(no_ord.get("ordinal").is_none());
}
