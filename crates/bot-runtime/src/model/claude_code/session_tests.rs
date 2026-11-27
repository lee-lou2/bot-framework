//! Claude Code session tests: fresh sessions, front-confirmed batch commits, resume, and symlink safety.

use super::*;
use crate::model::{
    FakeSandbox, Limits, Message, ModelStep, PortFuture, Role, Script, SessionPort, ToolOutput,
    session::{Pending, ToolUse},
};
use bot_core::{
    cli::{CliResponse, CliResponseUse},
    command::{JsonPayload, Step},
    id::*,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};

const CHAIN: &str = include_str!("../session/fixture.jsonl");
const TRUSTED: &str = r#"{"content":[{"type":"text","text":"ledger result"}],"isError":false}"#;
const OK: &str = r#"{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"D"}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"o"}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"n"}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"e"}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"."}}}
{"type":"result","subtype":"success","is_error":false,"api_error_status":null,"result":"Done.","usage":{"input_tokens":1,"output_tokens":1}}"#;

fn lines() -> Vec<&'static str> {
    CHAIN.lines().collect()
}

fn model() -> CliModel {
    CliModel {
        id: "test-model".into(),
        effort: "high".into(),
        max_output_tokens: 777,
        retries: 3,
        context_window: 1000,
    }
}

fn relay() -> RelayChannel {
    RelayChannel {
        name: "relay".into(),
        base_url: "http://192.168.64.1:8787".into(),
        token: "sess-token-1".into(),
    }
}

fn attempt() -> Attempt {
    Attempt {
        profile: ProfileId("personal".into()),
        run: RunId("r/1".into()),
        claim: Some(bot_core::id::ClaimEpoch(1)),
        generation: Generation(2),
    }
}

fn sid() -> String {
    session_id(&attempt().profile, &attempt().run)
}

fn request(port: Arc<FakePort>) -> ModelRequest {
    ModelRequest {
        attempt: attempt(),
        model: "assistant".into(),
        system: "You are an assistant.".into(),
        conversation: vec![Message {
            role: Role::User,
            speaker: Some(PrincipalId("local:owner".into())),
            text: "Delete it".into(),
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
        session: Some(Session(port)),
    }
}

fn runner(sandbox: &FakeSandbox, tag: &str) -> ClaudeCodeRunner {
    let ws = std::env::temp_dir().join(format!("bot-runtime-claude-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&ws);
    std::fs::create_dir_all(&ws).unwrap();
    let config = CliConfig::new(&ws).with_model("assistant", model());
    ClaudeCodeRunner::new(Arc::new(sandbox.clone()), config)
}

fn session_file(spec: &ModelSpec) -> PathBuf {
    spec.scratch
        .as_ref()
        .unwrap()
        .host
        .join(format!("claude/projects/-bot/{}.jsonl", sid()))
}

fn append(spec: &ModelSpec, text: &str) {
    use std::io::Write;
    let path = session_file(spec);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    writeln!(file, "{text}").unwrap();
}

fn probe_response() -> CliResponse {
    CliResponse {
        id: "msg_1".into(),
        uses: vec![CliResponseUse {
            id: "toolu_probe1".into(),
            name: "mcp__relay__nothing".into(),
            input: json!({"x": 1}),
        }],
    }
}

fn saved_steps(n: usize) -> Vec<Step> {
    let mut journal = Journal::new(0, Default::default(), "mcp__relay__".into());
    for line in &lines()[..n] {
        journal.feed(line);
    }
    journal.plan(&probe_response()).unwrap().steps
}

#[derive(Default)]
struct FakePort {
    steps: Vec<Step>,
    commits: Mutex<Vec<Vec<Step>>>,
    responses: Mutex<BTreeMap<String, CliResponse>>,
    /// Slots `(turn, pos)` still unfinished in the ledger; others resolve to `TRUSTED`.
    unfinished: Mutex<BTreeSet<(u64, u32)>>,
    /// Slots whose plan was saved (`recall` passes) but that never ran (`ended` returns false).
    never_ran: Mutex<BTreeSet<(u64, u32)>>,
    ended_hangs: std::sync::atomic::AtomicBool,
    settle: Mutex<VecDeque<Result<Slot, SessionError>>>,
    settled: Mutex<Vec<(u64, u32, String)>>,
    recall: Mutex<VecDeque<Option<ToolOutput>>>,
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

fn keys(steps: &[Step]) -> Vec<&str> {
    steps.iter().map(|s| s.key.as_str()).collect()
}

impl SessionPort for FakePort {
    fn steps(&self) -> &[Step] {
        &self.steps
    }
    fn commit(&self, steps: Vec<Step>) -> PortFuture<'_, ()> {
        let error = *self.commit_error.lock().unwrap();
        if error.is_none() {
            self.commits.lock().unwrap().push(steps);
        }
        Box::pin(async move { error.map_or(Ok(()), Err) })
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
        let next = self.recall.lock().unwrap().pop_front();
        let finished = !self
            .unfinished
            .lock()
            .unwrap()
            .contains(&(slot.turn, slot.pos));
        Box::pin(async move {
            Ok(match next {
                Some(output) => output,
                None => finished.then(|| ToolOutput::from_payload(TRUSTED)),
            })
        })
    }
    fn ended<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, bool> {
        let ran = !self
            .never_ran
            .lock()
            .unwrap()
            .contains(&(slot.turn, slot.pos))
            && !self
                .unfinished
                .lock()
                .unwrap()
                .contains(&(slot.turn, slot.pos));
        let hangs = self.ended_hangs.load(std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move {
            if hangs {
                std::future::pending::<()>().await;
            }
            Ok(ran)
        })
    }
    fn settle<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, Slot> {
        self.settled
            .lock()
            .unwrap()
            .push((slot.turn, slot.pos, slot.tool_use.id.clone()));
        let next = self.settle.lock().unwrap().pop_front();
        Box::pin(async move { next.unwrap_or(Ok(Slot::Result(ToolOutput::notice("aborted")))) })
    }
}

fn slow() -> Script {
    Script::output(OK).slow(Duration::from_millis(40))
}

fn sandbox_writing(n: usize) -> FakeSandbox {
    FakeSandbox::new([slow()]).inspect(move |spec| {
        for line in &lines()[..n] {
            append(spec, line);
        }
    })
}

#[tokio::test]
async fn a_fresh_session_is_named_and_one_batch_is_written_once_the_front_confirmed_the_response() {
    let port = FakePort::new(vec![]);
    port.confirm(probe_response());
    let sandbox = sandbox_writing(4);
    let out = runner(&sandbox, "fresh")
        .step(request(port.clone()))
        .await
        .unwrap();
    assert!(matches!(out, ModelStep::Final { ref text, .. } if text == "Done."));
    let spec = &sandbox.specs()[0];
    let at = spec.args.iter().position(|a| a == "--session-id").unwrap();
    assert_eq!(spec.args[at + 1], sid());
    assert!(
        !spec
            .args
            .iter()
            .any(|a| a == "--no-session-persistence" || a == "--resume")
    );
    assert!(
        spec.stdin.is_some(),
        "initial session sends conversation on stdin"
    );
    let commits = port.commits();
    assert_eq!(commits.len(), 1, "one response commits as one batch");
    assert_eq!(
        keys(&commits[0]),
        [
            "cli-rec-0",
            "cli-rec-1",
            "cli-rec-2",
            "cli-rec-3",
            "cli-use-toolu_probe1",
            "cli-group-3"
        ]
    );
}

#[tokio::test]
async fn nothing_is_written_until_the_front_confirms_and_a_late_confirmation_still_commits() {
    let port = FakePort::new(vec![]);
    let sandbox = sandbox_writing(4);
    let late = port.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            late.commits().is_empty(),
            "nothing may commit before front confirmation"
        );
        late.confirm(probe_response());
    });
    runner(&sandbox, "late")
        .step(request(port.clone()))
        .await
        .unwrap();
    assert_eq!(port.commits().len(), 1);
}

#[tokio::test]
async fn a_response_that_differs_from_the_front_confirmation_stops_the_cli_and_writes_nothing() {
    let port = FakePort::new(vec![]);
    let mut wrong = probe_response();
    wrong.uses[0].input = json!({"x": 2});
    port.confirm(wrong);
    let sandbox = FakeSandbox::new([slow().hangs()]).inspect(|spec| {
        for line in &lines()[..4] {
            append(spec, line);
        }
    });
    let error = runner(&sandbox, "mismatch")
        .step(request(port.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ModelError::Failed {
            code: "session_mismatch",
            transient: false
        }
    );
    assert!(port.commits().is_empty());
    assert_eq!(sandbox.killed(), 1, "untrusted session CLI must be killed");
}

#[tokio::test]
async fn losing_ownership_while_writing_ends_the_run_as_lost() {
    let port = FakePort::new(vec![]);
    port.confirm(probe_response());
    *port.commit_error.lock().unwrap() = Some(SessionError::Lost);
    let sandbox = FakeSandbox::new([slow().hangs()]).inspect(|spec| {
        for line in &lines()[..4] {
            append(spec, line);
        }
    });
    let error = runner(&sandbox, "lost")
        .step(request(port))
        .await
        .unwrap_err();
    assert_eq!(error, ModelError::Lost);
    assert_eq!(sandbox.killed(), 1);
}

mod finish;
mod restore;

#[tokio::test]
async fn a_line_that_never_ends_or_too_many_unconfirmed_lines_stop_the_journal_without_growing_memory()
 {
    let port = FakePort::new(vec![]);
    port.confirm(probe_response());
    let sandbox = FakeSandbox::new([slow()]).inspect(|spec| {
        use std::io::Write;
        let path = session_file(spec);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(
            "x".repeat(crate::model::session::MAX_LINE_BYTES + 10)
                .as_bytes(),
        )
        .unwrap();
    });
    let out = runner(&sandbox, "huge")
        .step(request(port.clone()))
        .await
        .unwrap();
    assert!(matches!(out, ModelStep::Final { .. }));
    assert!(port.commits().is_empty());
}

#[tokio::test]
async fn resuming_parks_again_without_starting_a_cli_when_a_slot_must_wait() {
    let port = FakePort::new(saved_steps(4));
    port.settle.lock().unwrap().push_back(Ok(Slot::Parked));
    let sandbox = FakeSandbox::new([slow()]);
    let error = runner(&sandbox, "parked")
        .step(request(port.clone()))
        .await
        .unwrap_err();
    assert_eq!(error, ModelError::Parked);
    assert!(sandbox.specs().is_empty(), "CLI must not start");
    assert!(port.commits().is_empty(), "partial results must not commit");
}

#[tokio::test]
async fn slot_failures_map_to_model_errors_and_corrupt_records_never_start_a_cli() {
    for (outcome, want) in [
        (SessionError::Lost, ModelError::Lost),
        (
            SessionError::Unavailable,
            ModelError::Failed {
                code: "session_unavailable",
                transient: true,
            },
        ),
        (
            SessionError::Permanent("checkpoint_conflict"),
            ModelError::Failed {
                code: "checkpoint_conflict",
                transient: false,
            },
        ),
    ] {
        let port = FakePort::new(saved_steps(4));
        port.settle.lock().unwrap().push_back(Err(outcome));
        let sandbox = FakeSandbox::new([slow()]);
        let error = runner(&sandbox, "errors")
            .step(request(port))
            .await
            .unwrap_err();
        assert_eq!(error, want);
        assert!(sandbox.specs().is_empty());
    }
    let broken: Vec<Step> = saved_steps(4)
        .into_iter()
        .filter(|s| !s.key.starts_with("cli-rec-"))
        .collect();
    let sandbox = FakeSandbox::new([slow()]);
    let error = runner(&sandbox, "corrupt")
        .step(request(FakePort::new(broken)))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ModelError::Failed {
            code: "session_corrupt",
            transient: false
        }
    );
    assert!(sandbox.specs().is_empty());
    let _ = ToolUse {
        id: String::new(),
        name: String::new(),
        input: json!(null),
    };
    let _ = JsonPayload(String::new());
}

#[tokio::test]
async fn a_session_path_the_vm_swapped_for_a_link_is_never_followed_and_ends_the_run() {
    use std::os::unix::fs::symlink;
    let port = FakePort::new(vec![]);
    port.confirm(probe_response());
    let outside = std::env::temp_dir().join(format!("bot-outside-session-{}", std::process::id()));
    std::fs::write(&outside, lines()[..4].join("\n") + "\n").unwrap();
    let target = outside.clone();
    let sandbox = FakeSandbox::new([slow().hangs()]).inspect(move |spec| {
        let path = session_file(spec);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        symlink(&target, path).unwrap();
    });
    let error = runner(&sandbox, "swapped")
        .step(request(port.clone()))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ModelError::Failed {
            code: "session_unsafe",
            transient: false
        }
    );
    assert!(
        port.commits().is_empty(),
        "external file contents must not be committed"
    );
    assert_eq!(sandbox.killed(), 1, "untrusted session CLI must be killed");
    let _ = std::fs::remove_file(outside);
}
