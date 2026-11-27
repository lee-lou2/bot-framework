//! Unit tests for AGY CLI argument assembly, 22 KB schema guard, token confinement, stream parsing, and ledger resume.

use super::*;
use crate::model::{
    FakeSandbox, Limits, Message, ModelStep, PortFuture, Role, Script, SessionError, SessionPort,
    ToolOutput,
    sandbox::files_holding,
    session::{Pending, Session, Slot},
};
use bot_core::{
    cli::{CliGroup, CliUse, GROUP_PREFIX, USE_PREFIX},
    command::{JsonPayload, Step},
    id::*,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};
use tokio::sync::watch;

const OK: &str = r#"{"event":"init","session_id":"conv-1"}
{"event":"step_update","step_update":{"step_type":"agent_response","state":"ACTIVE","text_delta":""}}
{"event":"step_update","step_update":{"step_type":"agent_response","state":"ACTIVE","text_delta":"1+1 is 2."}}
{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE","usage":{"input_tokens":150,"output_tokens":20}}}
{"event":"result","result":{"status":"SUCCESS","response":"1+1 is 2.\n","usage":{"input_tokens":150,"output_tokens":20}}}"#;

const ERR_429: &str = r#"{"event":"step_update","step_update":{"step_type":"error_message","state":"ERROR","error":"429 rate limit exceeded"}}
{"event":"result","result":{"status":"ERROR","error":"429 rate limit exceeded"}}"#;

fn model(effort: &str) -> CliModel {
    CliModel {
        id: "gemini-3.1-pro-preview".into(),
        effort: effort.into(),
        max_output_tokens: 4096,
        retries: 2,
        context_window: 1_000_000,
    }
}

fn relay() -> RelayChannel {
    RelayChannel {
        name: "relay".into(),
        base_url: "http://192.168.64.1:8787".into(),
        token: "sess-token-agy-1".into(),
    }
}

fn sample_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            id: ToolId("agenda.list".into()),
            kind: ToolKind::Skill,
            class: Risk::Read,
            description: "List scheduled events.".into(),
            input: JsonPayload(
                r#"{"type":"object","properties":{"date":{"type":"string"}}}"#.into(),
            ),
        },
        ToolSpec {
            id: ToolId("delegate".into()),
            kind: ToolKind::Delegate,
            class: Risk::Read,
            description: "Delegate to child agent.".into(),
            input: JsonPayload(r#"{"type":"object","required":["agent","instruction"]}"#.into()),
        },
    ]
}

fn workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bot-runtime-agy-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn request(session: Option<Session>) -> ModelRequest {
    ModelRequest {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r-agy-1".into()),
            claim: Some(ClaimEpoch(1)),
            generation: Generation(1),
        },
        model: "assistant".into(),
        system: "You are an AGY assistant.".into(),
        conversation: vec![Message {
            role: Role::User,
            speaker: Some(PrincipalId("local:owner".into())),
            text: "What is 1+1?".into(),
        }],
        tools: sample_tools(),
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
    let mut tracker = AgyTracker::new(resumed);
    let events = sample.lines().filter_map(|l| tracker.feed(l)).collect();
    (events, Box::new(tracker).finish())
}

#[test]
fn parses_agy_stream_events_usage_and_errors() {
    let (events, out) = feed_all(OK, false);
    assert_eq!(events, [Event::Thinking, Event::Writing, Event::Thinking]);
    let out = out.unwrap();
    assert_eq!(out.text, "1+1 is 2.");
    assert_eq!(
        out.usage,
        Usage {
            input_tokens: 150,
            output_tokens: 20
        }
    );
    assert_eq!(
        out.outcome.usage,
        Some(TokenUsage {
            prompt: 150,
            completion: 20
        })
    );
    assert_eq!(feed_all(OK, true).1.unwrap().outcome.usage, None);

    assert_eq!(
        feed_all(ERR_429, false).1,
        Err(ModelError::Failed {
            code: "rate_limited",
            transient: true
        })
    );
    assert_eq!(
        feed_all(
            r#"{"event":"result","result":{"status":"SUCCESS","response":"  "}}"#,
            false
        )
        .1,
        Err(ModelError::Failed {
            code: "empty_answer",
            transient: false
        })
    );
    assert_eq!(
        feed_all(r#"{"event":"init"}"#, false).1,
        Err(ModelError::Failed {
            code: "no_result",
            transient: false
        })
    );
}

#[test]
fn catalog_model_and_args_enforce_stream_json_and_refuse_builtins() {
    assert_eq!(
        AgyRunner::catalog_model(&model("low")),
        "Gemini 3.1 Pro (Low)"
    );
    assert_eq!(
        AgyRunner::catalog_model(&model("high")),
        "Gemini 3.1 Pro (High)"
    );
    let explicit = CliModel {
        id: "Gemini 3.1 Pro (Low)".into(),
        ..model("high")
    };
    assert_eq!(AgyRunner::catalog_model(&explicit), "Gemini 3.1 Pro (Low)");

    let args = AgyRunner::args(&model("low"), &BTreeSet::new()).unwrap();
    assert_eq!(
        args,
        [
            "--print=",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--disable-slash-commands",
            "--model",
            "Gemini 3.1 Pro (Low)",
            "--settings",
            "/work/agy.settings.json",
        ]
    );
    let hydrogen = CliModel {
        id: "gemini-4-hydrogen".into(),
        ..model("xhigh")
    };
    assert_eq!(AgyRunner::catalog_model(&hydrogen), "gemini-4-hydrogen");
    assert_eq!(
        AgyRunner::args(&hydrogen, &BTreeSet::new()).unwrap(),
        [
            "--print=",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--disable-slash-commands",
            "--model",
            "gemini-4-hydrogen",
            "--effort",
            "xhigh",
            "--settings",
            "/work/agy.settings.json",
        ]
    );
    let suffixed = CliModel {
        id: "gemini-4-hydrogen-xhigh".into(),
        ..model("xhigh")
    };
    assert!(
        !AgyRunner::args(&suffixed, &BTreeSet::new())
            .unwrap()
            .iter()
            .any(|a| a == "--effort")
    );
    assert_eq!(
        AgyRunner::args(&model("low"), &BTreeSet::from([Builtin::Read])).unwrap_err(),
        ModelError::Failed {
            code: "unsupported_builtin",
            transient: false
        }
    );
}

#[test]
fn layout_instructions_keeps_agents_md_under_22kb_and_spills_schemas_or_overflow_cleanly() {
    let tools = sample_tools();
    let (agents, spill) = layout_instructions("Short system.", "relay", &tools);
    assert!(agents.len() <= MAX_AGENTS_BYTES);
    assert!(agents.contains("Short system."));
    assert!(agents.contains("`agenda_list`"));
    assert!(agents.contains("`delegate`"));
    assert!(spill.is_none());

    // System fits under 22 KB, but system + tools exceeds 22 KB: tools spill to stdin.
    let big_system = "s".repeat(MAX_AGENTS_BYTES - 50);
    let (agents, spill) = layout_instructions(&big_system, "relay", &tools);
    assert_eq!(agents, big_system);
    assert!(agents.len() <= MAX_AGENTS_BYTES);
    let spill = spill.expect("tools should spill into stdin");
    assert!(spill.contains("`agenda_list`"));
    assert!(spill.contains("`delegate`"));

    // System itself exceeds 22 KB (with multibyte UTF-8): split on valid char boundary and spill tail + tools.
    let huge_system = format!("{}\n{}", "€".repeat(8_000), "TAIL_SENTINEL");
    assert!(huge_system.len() > MAX_AGENTS_BYTES);
    let (agents, spill) = layout_instructions(&huge_system, "relay", &tools);
    assert!(agents.len() <= MAX_AGENTS_BYTES);
    let spill = spill.expect("overflow and tools must spill into stdin");
    assert!(spill.contains("TAIL_SENTINEL"));
    assert!(spill.contains("`agenda_list`"));
}

#[tokio::test]
async fn step_writes_template_settings_symlink_keeps_token_off_disk_and_rejects_builtins() {
    let ws = workspace("spec");
    let captured = Arc::new(Mutex::new((
        Vec::<PathBuf>::new(),
        String::new(),
        String::new(),
        Option::<PathBuf>::None,
        String::new(),
    )));
    let sink = captured.clone();
    let sandbox = FakeSandbox::new([Script::output(OK)]).inspect(move |spec| {
        let input = &spec.readonly_mounts[0].host;
        let work = &spec.scratch.as_ref().unwrap().host;
        let hits = files_holding(&[input, work], "sess-token-agy-1");
        let settings = std::fs::read_to_string(work.join(SETTINGS_CONFIG)).unwrap();
        let template = std::fs::read_to_string(work.join(MCP_TEMPLATE)).unwrap();
        let link = std::fs::read_link(work.join(MCP_LINK_RELATIVE)).ok();
        let agents = std::fs::read_to_string(input.join("AGENTS.md")).unwrap();
        *sink.lock().unwrap() = (hits, settings, template, link, agents);
    });
    let runner = AgyRunner::new(
        Arc::new(sandbox.clone()),
        CliConfig::new(&ws).with_model("assistant", model("low")),
    );
    assert_eq!(runner.kind(), RunnerKind::Loop);
    assert_eq!(
        runner.meta("assistant"),
        Some(ModelMeta {
            provider: "google",
            model: "gemini-3.1-pro-preview".into(),
        })
    );

    let out = runner.step(request(None)).await.unwrap();
    assert!(matches!(out, ModelStep::Final { ref text, .. } if text == "1+1 is 2."));

    let (hits, settings, template, link, agents) = captured.lock().unwrap().clone();
    assert!(
        hits.is_empty(),
        "session token must never touch mounted files: {hits:?}"
    );
    let settings_v: Value = serde_json::from_str(&settings).unwrap();
    assert_eq!(settings_v["modelProvider"], "gemini");
    assert_eq!(settings_v["toolPermission"], "always-proceed");
    assert_eq!(settings_v["permissions"]["allow"], json!(["mcp(relay/*)"]));
    let tpl_v: Value = serde_json::from_str(&template).unwrap();
    assert_eq!(
        tpl_v["mcpServers"]["relay"]["headers"]["Authorization"],
        "Bearer ${RELAY_TOKEN}"
    );
    assert_eq!(link.as_deref(), Some(std::path::Path::new(MCP_LINK_TARGET)));
    assert!(agents.contains("`agenda_list`"));
    assert!(agents.len() <= MAX_AGENTS_BYTES);

    let spec = sandbox.specs().remove(0);
    assert_eq!(spec.program, "agy");
    assert!(spec.args.iter().all(|a| !a.contains("sess-token-agy-1")));
    let carrying: Vec<&str> = spec
        .env
        .iter()
        .filter(|(_, v)| v.contains("sess-token-agy-1"))
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(carrying, ["GEMINI_API_KEY", "RELAY_TOKEN"]);
    let stdin_line = String::from_utf8(spec.stdin.unwrap()).unwrap();
    assert!(stdin_line.ends_with('\n'));
    let stdin_v: Value = serde_json::from_str(&stdin_line).unwrap();
    assert_eq!(stdin_v["event"], "user");
    assert!(
        stdin_v["message"]["content"]
            .as_str()
            .unwrap()
            .contains("What is 1+1?")
    );

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
    recalled: Mutex<BTreeMap<(u64, u32), Option<ToolOutput>>>,
    settle: Mutex<VecDeque<Result<Slot, SessionError>>>,
}

impl SessionPort for FakePort {
    fn steps(&self) -> &[Step] {
        &self.steps
    }

    fn commit(&self, _steps: Vec<Step>) -> PortFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn response(&self, _message_id: &str) -> PortFuture<'_, Option<bot_core::cli::CliResponse>> {
        Box::pin(async { Ok(None) })
    }

    fn recall<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, Option<ToolOutput>> {
        let out = self
            .recalled
            .lock()
            .unwrap()
            .get(&(slot.turn, slot.pos))
            .cloned()
            .flatten();
        Box::pin(async move { Ok(out) })
    }

    fn ended<'a>(&'a self, _slot: &'a Pending) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }

    fn settle<'a>(&'a self, _slot: &'a Pending) -> PortFuture<'a, Slot> {
        let next = self.settle.lock().unwrap().pop_front();
        Box::pin(async move { next.unwrap_or(Ok(Slot::Result(ToolOutput::notice("aborted")))) })
    }
}

fn agy_steps() -> Vec<Step> {
    vec![
        Step {
            key: format!("{USE_PREFIX}agy-0-0"),
            body: JsonPayload(
                serde_json::to_string(&CliUse {
                    seq: 0,
                    turn: 0,
                    pos: 0,
                    name: "mcp__relay__agenda_list".into(),
                    input: json!({"date": "2026-11-27"}),
                })
                .unwrap(),
            ),
        },
        Step {
            key: format!("{GROUP_PREFIX}0"),
            body: JsonPayload(
                serde_json::to_string(&CliGroup {
                    seq: 0,
                    uses: vec!["agy-0-0".into()],
                })
                .unwrap(),
            ),
        },
        Step {
            key: format!("{USE_PREFIX}agy-1-0"),
            body: JsonPayload(
                serde_json::to_string(&CliUse {
                    seq: 1,
                    turn: 1,
                    pos: 0,
                    name: "mcp__relay__agenda_delete".into(),
                    input: json!({"id": "e1"}),
                })
                .unwrap(),
            ),
        },
        Step {
            key: format!("{GROUP_PREFIX}1"),
            body: JsonPayload(
                serde_json::to_string(&CliGroup {
                    seq: 1,
                    uses: vec!["agy-1-0".into()],
                })
                .unwrap(),
            ),
        },
    ]
}

#[tokio::test]
async fn resume_recalls_and_settles_broker_turns_into_stdin_exchanges_and_parks_without_vm() {
    let ws = workspace("resume");

    // Parked slot returns Parked without launching AGY.
    let parked_port = Arc::new(FakePort {
        steps: agy_steps(),
        recalled: Mutex::new(BTreeMap::from([(
            (0, 0),
            Some(ToolOutput::from_payload(r#"{"events":["Standup"]}"#)),
        )])),
        settle: Mutex::new(VecDeque::from([Ok(Slot::Parked)])),
    });
    let idle_sandbox = FakeSandbox::new([Script::output(OK)]);
    let runner = AgyRunner::new(
        Arc::new(idle_sandbox.clone()),
        CliConfig::new(&ws).with_model("assistant", model("low")),
    );
    assert_eq!(
        runner
            .step(request(Some(Session(parked_port))))
            .await
            .unwrap_err(),
        ModelError::Parked
    );
    assert!(idle_sandbox.specs().is_empty());

    // Settled slot injects trusted Exchange history into stdin and suppresses partial usage.
    let port = Arc::new(FakePort {
        steps: agy_steps(),
        recalled: Mutex::new(BTreeMap::from([(
            (0, 0),
            Some(ToolOutput::from_payload(r#"{"events":["Standup"]}"#)),
        )])),
        settle: Mutex::new(VecDeque::from([Ok(Slot::Result(ToolOutput::notice(
            "denied",
        )))])),
    });
    let sandbox = FakeSandbox::new([Script::output(OK)]);
    let runner = AgyRunner::new(
        Arc::new(sandbox.clone()),
        CliConfig::new(&ws).with_model("assistant", model("low")),
    );
    let step = runner.step(request(Some(Session(port)))).await.unwrap();
    match step {
        ModelStep::Final { text, outcome, .. } => {
            assert_eq!(text, "1+1 is 2.");
            assert_eq!(outcome.usage, None);
        }
        other => panic!("unexpected step: {other:?}"),
    }
    let spec = sandbox.specs().remove(0);
    let stdin_v: Value =
        serde_json::from_str(&String::from_utf8(spec.stdin.unwrap()).unwrap()).unwrap();
    let content = stdin_v["message"]["content"].as_str().unwrap();
    assert!(
        content.contains("# Completed steps in this request"),
        "{content}"
    );
    assert!(content.contains("`agenda.list`"), "{content}");
    assert!(content.contains("Standup"), "{content}");
    assert!(content.contains("`agenda.delete`"), "{content}");
    assert!(
        content.contains("Not executed: approval denied or blocked."),
        "{content}"
    );

    // Corrupt checkpoint (group missing matching use) fails closed without launching VM.
    let bad_port = Arc::new(FakePort {
        steps: vec![Step {
            key: format!("{GROUP_PREFIX}0"),
            body: JsonPayload(
                serde_json::to_string(&CliGroup {
                    seq: 0,
                    uses: vec!["agy-0-0".into()],
                })
                .unwrap(),
            ),
        }],
        ..FakePort::default()
    });
    let idle_sandbox = FakeSandbox::new([Script::output(OK)]);
    let runner = AgyRunner::new(
        Arc::new(idle_sandbox.clone()),
        CliConfig::new(&ws).with_model("assistant", model("low")),
    );
    assert_eq!(
        runner
            .step(request(Some(Session(bad_port))))
            .await
            .unwrap_err(),
        ModelError::Failed {
            code: "session_corrupt",
            transient: false
        }
    );
    assert!(idle_sandbox.specs().is_empty());
    let _ = std::fs::remove_dir_all(&ws);
}
