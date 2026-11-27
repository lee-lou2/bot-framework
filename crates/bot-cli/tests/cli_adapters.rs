//! End-to-end integration tests for `codex-cli` and `agy-cli` host wiring, relay fronts, and approval resume.

use bot_broker::{
    front::{cli::PlannedCalls, observe::CheckpointSink, park::ParkReason, *},
    model::{ModelLimits, ModelProxy, Protocol},
    ports::Ran,
    service::*,
    testing::*,
};
use bot_cli::{host::parse as parse_host, serve::claude::model_host};
use bot_core::{
    canon::canonicalize, cli::*, command::*, id::*, intake::*, ledger::*, port::*, sandbox::*,
    state::*,
};
use bot_runtime::model::{session::Pending, *};
use reqwest::Client;
use serde_json::Value;
use std::{collections::*, fs, future::Future, path::Path, pin::Pin, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio::sync::{mpsc, watch};

const ALICE: &str = "local:alice";
const OPENAI_KEY: &str = "sk-openai-host-secret";
const GEMINI_KEY: &str = "AIza-gemini-host-secret";
const HOST_JSON: &str = r#"{"version":1,"models":{"acme":["codex","agy"]},"providers":{"codex":{"kind":"codex-cli","model":"gpt-5.4","endpoint":"https://api.openai.com/v1","secret":"openai-key","effort":"high"},"agy":{"kind":"agy-cli","model":"gemini-3.1-pro-preview","endpoint":"https://generativelanguage.googleapis.com","secret":"gemini-key","effort":"high"}}}"#;
const GEMINI_PATH: &str = "/v1beta/models/gemini-3.1-pro-preview:streamGenerateContent?alt=sse";
const CODEX_REQ: &str = r#"{"model":"client-ignored","stream":true,"input":[{"type":"message","role":"user","content":"hi"}]}"#;
const GEMINI_REQ: &str = r#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#;
const SCOPES: [&str; 2] = ["echo.read", "echo.drop"];

async fn commit_steps(l: Arc<MemLedger>, a: Attempt, steps: Vec<Step>) -> Result<(), Error> {
    let _ = l.context(a.clone()).await?;
    for s in steps {
        l.add_step(&a.run.0, &s.key, &s.body.0);
    }
    Ok(())
}

macro_rules! stub {
    ($($f:ident($($a:ty),*) -> $r:ty;)*) => { $(fn $f(&self, $(_: $a),*) -> Reply<'_, $r> { unimplemented!() })* };
}

struct MemRuns(Arc<MemLedger>);

impl RunStore for MemRuns {
    stub! {
        accept_with(Accept, AcceptExtras) -> RunId; status(RequestKey) -> Option<RequestState>; progress(ProfileId, RunId) -> Option<RunProgress>;
        claim() -> Option<RunContext>; release_for_shutdown(Attempt) -> (); renew(Attempt) -> Beat;
        finish(Attempt, Finish) -> Completion; finish_with(Attempt, Finish, RunOutcome) -> Completion; fail(Attempt, Failure) -> Retry; history(Attempt, u32) -> Vec<Turn>;
    }
    fn checkpoint_batch(&self, a: Attempt, steps: Vec<Step>) -> Reply<'_, ()> {
        Box::pin(commit_steps(self.0.clone(), a, steps))
    }
}

struct LedgerSession(Vec<Step>, Attempt, Arc<MemLedger>, Arc<BrokerService>);

fn se(e: Error) -> SessionError {
    if matches!(e, Error::Stale | Error::Denied | Error::Missing) {
        SessionError::Lost
    } else {
        SessionError::Unavailable
    }
}

impl SessionPort for LedgerSession {
    fn steps(&self) -> &[Step] {
        &self.0
    }
    fn commit(&self, steps: Vec<Step>) -> PortFuture<'_, ()> {
        let (l, a) = (self.2.clone(), self.1.clone());
        Box::pin(async move { commit_steps(l, a, steps).await.map_err(se) })
    }
    fn response(&self, id: &str) -> PortFuture<'_, Option<CliResponse>> {
        let (a, key) = (self.1.clone(), format!("{RESPONSE_PREFIX}{id}"));
        Box::pin(async move {
            let b = self.2.step(a, key).await.map_err(se)?;
            Ok(b.map(|p| serde_json::from_str(&p.0).unwrap()))
        })
    }
    fn recall<'a>(&'a self, s: &'a Pending) -> PortFuture<'a, Option<ToolOutput>> {
        Box::pin(async move {
            let rec = self
                .2
                .lookup(self.1.clone(), CallId::cli(s.turn, s.pos))
                .await
                .map_err(se)?;
            Ok(match rec.map(|r| r.phase) {
                Some(CallPhase::Done(p)) => Some(ToolOutput::from_payload(&p.0)),
                Some(CallPhase::Denied) => Some(ToolOutput::notice("denied")),
                _ => None,
            })
        })
    }
    fn ended<'a>(&'a self, s: &'a Pending) -> PortFuture<'a, bool> {
        Box::pin(async move { Ok(self.recall(s).await?.is_some()) })
    }
    fn settle<'a>(&'a self, s: &'a Pending) -> PortFuture<'a, Slot> {
        Box::pin(async move {
            let mcp = s.tool_use.name.strip_prefix("mcp__relay__").unwrap();
            let call = ToolCall {
                attempt: self.1.clone(),
                call: CallId::cli(s.turn, s.pos),
                tool: ToolId::from_mcp_name(mcp).unwrap(),
                args: JsonPayload(canonicalize(&s.tool_use.input).unwrap()),
            };
            match self.3.call(call).await {
                Ok(CallResult::Done(p)) => Ok(Slot::Result(ToolOutput::from_payload(&p.0))),
                Ok(CallResult::Awaiting(_) | CallResult::Held(_)) => Ok(Slot::Parked),
                _ => Err(SessionError::Unavailable),
            }
        })
    }
}

type ScriptFn = Box<
    dyn FnOnce(ModelSpec, mpsc::UnboundedSender<String>) -> Pin<Box<dyn Future<Output = ()> + Send>>
        + Send,
>;

#[derive(Default)]
struct ScriptedSandbox(std::sync::Mutex<VecDeque<ScriptFn>>);

struct ScriptedProcess(
    mpsc::UnboundedReceiver<String>,
    Option<tokio::task::JoinHandle<()>>,
);

impl ModelProcess for ScriptedProcess {
    fn next_line(&mut self) -> LineFuture<'_> {
        Box::pin(async move { self.0.recv().await })
    }
    fn kill(&mut self) -> UnitFuture<'_> {
        let _ = self.1.take().map(|h| h.abort());
        Box::pin(async {})
    }
    fn wait(&mut self) -> ExitFuture<'_> {
        Box::pin(async move {
            let code = match self.1.take() {
                Some(h) => h.await.ok().map(|()| 0),
                None => None,
            };
            let stderr_tail = String::new();
            Exit { code, stderr_tail }
        })
    }
}

impl Sandbox<ModelSpec> for ScriptedSandbox {
    type Process = Box<dyn ModelProcess>;
    fn spawn(&self, spec: ModelSpec) -> Reply<'_, Self::Process> {
        let f = self.0.lock().unwrap().pop_front().expect("scripted run");
        let (tx, rx) = mpsc::unbounded_channel();
        let p: Self::Process = Box::new(ScriptedProcess(rx, Some(tokio::spawn(f(spec, tx)))));
        Box::pin(async move { Ok(p) })
    }
    fn reap(&self, _: &str) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct Rig {
    front: ModelFront,
    sessions: Arc<SessionTable>,
    upstream: Arc<FakeUpstream>,
    ledger: Arc<MemLedger>,
    runner: Arc<FakeRunner>,
    broker: Arc<BrokerService>,
    park: Arc<RecordingPark>,
    sb: Arc<ScriptedSandbox>,
    cfg: CliConfig,
    _dir: TempDir,
}

impl Rig {
    async fn start() -> Self {
        let mh = model_host(&parse_host(HOST_JSON).unwrap()).unwrap();
        let clk = Arc::new(ManualClock::new(1_700_000_000));
        let ent = Arc::new(CountingEntropy::default());
        let cat = Arc::new(FakeCatalog::new());
        let drop_tool = tool(Risk::Irreversible, &["echo.drop"]);
        cat.publish(
            snapshot("acme", "r1")
                .policy_scopes(&SCOPES)
                .approve(&["echo.drop"])
                .role("ops", &SCOPES)
                .member(ALICE, &["ops"])
                .tool("main", "echo.say", tool(Risk::Read, &["echo.read"]))
                .tool("main", "echo.drop", drop_tool)
                .build(),
        );
        let ledger = Arc::new(MemLedger::new(clk.clone()));
        ledger.set_active_revision("acme", "r1");
        let runner = Arc::new(FakeRunner::new());
        let bp = Parts {
            catalog: cat.clone(),
            ledger: ledger.clone(),
            effects: Arc::new(FakeEffects::new()),
            memory: Arc::new(MemMemory::new(ledger.clone())),
            audit: Arc::new(MemAudit::new()),
            runner: runner.clone(),
            sealer: Arc::new(FakeSealer),
            tracer: Arc::new(RecordingTracer::new()),
            clock: clk.clone(),
            entropy: ent.clone(),
        };
        let broker = Arc::new(BrokerService::new(bp, Config::default()));
        let upstream = Arc::new(FakeUpstream::new());
        let sec = Arc::new(FakeSecrets::new(&[
            ("acme", "openai-key", OPENAI_KEY),
            ("acme", "gemini-key", GEMINI_KEY),
        ]));
        let (l, u) = (ledger.clone(), upstream.clone());
        let model = Arc::new(ModelProxy::new(mh, l, sec, u, clk, ModelLimits::default()));
        let sessions = SessionTable::new(ent);
        let turns = Arc::new(CheckpointSink::new(Arc::new(MemRuns(ledger.clone()))));
        let pc = PlannedCalls::new(ledger.clone(), Duration::from_secs(5))
            .with_poll(Duration::from_millis(10))
            .with_preflight(broker.clone());
        let park = Arc::new(RecordingPark::new());
        let fp = FrontParts {
            sessions: sessions.clone(),
            model,
            broker: broker.clone(),
            catalog: cat,
            calls: Arc::new(pc),
            turns,
            ledger: ledger.clone(),
            delegation: Arc::new(FakeDelegation::new()),
            park: park.clone(),
        };
        let front = ModelFront::start(([127, 0, 0, 1], 0).into(), fp, Default::default())
            .await
            .unwrap();
        let dir = TempDir::new().unwrap();
        let mk = |id: &str| CliModel {
            id: id.into(),
            effort: "high".into(),
            max_output_tokens: 8192,
            retries: 1,
            context_window: 200_000,
        };
        let cfg = CliConfig::new(dir.path())
            .with_model("codex", mk("gpt-5.4"))
            .with_model("agy", mk("gemini-3.1-pro-preview"));
        let sb = Arc::new(ScriptedSandbox::default());
        Self {
            front,
            sessions,
            upstream,
            ledger,
            runner,
            broker,
            park,
            sb,
            cfg,
            _dir: dir,
        }
    }

    fn base(&self) -> String {
        format!("http://{}", self.front.addr())
    }
    fn push<F, Fut>(&self, f: F)
    where
        F: FnOnce(ModelSpec, mpsc::UnboundedSender<String>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let mut q = self.sb.0.lock().unwrap();
        q.push_back(Box::new(move |s, tx| Box::pin(f(s, tx))));
    }
    fn add_run(&self, run: &str) -> Attempt {
        let ctx = run_context("acme", run, 1, ALICE, "main", &SCOPES);
        let a = ctx.attempt.clone();
        self.ledger.add_run(ctx);
        a
    }
    fn issue(&self, a: &Attempt, alias: &str, protocol: Protocol) -> RelayChannel {
        let s = bot_broker::front::Session {
            attempt: a.clone(),
            agent: AgentId("main".into()),
            alias: alias.into(),
            protocol,
        };
        let token = self.sessions.issue(s).unwrap();
        RelayChannel {
            name: "relay".into(),
            base_url: self.base(),
            token,
        }
    }
    fn queue_sse(&self, body: &[u8]) {
        self.upstream.respond_chunks(200, &[body], End::Eof);
    }
    fn script_ok(&self, json: &str) {
        self.runner.script(Ran::Done(JsonPayload(json.into())));
    }
    async fn step(
        &self,
        a: Attempt,
        alias: &str,
        cancel: Option<watch::Receiver<bool>>,
    ) -> Result<ModelStep, ModelError> {
        let is_codex = alias == "codex";
        let proto = [Protocol::Gemini, Protocol::OpenAiResponses][usize::from(is_codex)];
        let steps = self.ledger.context(a.clone()).await.unwrap().work.steps;
        let ls = LedgerSession(steps, a.clone(), self.ledger.clone(), self.broker.clone());
        let req = ModelRequest {
            relay: Some(self.issue(&a, alias, proto)),
            attempt: a,
            model: alias.into(),
            system: "Use relay tools.".into(),
            conversation: Vec::new(),
            tools: Vec::new(),
            exchanges: Vec::new(),
            limits: Limits::default(),
            cancel: cancel.unwrap_or_else(|| watch::channel(false).1),
            builtin: BTreeSet::new(),
            session: Some(bot_runtime::model::Session(Arc::new(ls))),
        };
        let (sb, cfg) = (self.sb.clone(), self.cfg.clone());
        if is_codex {
            CodexRunner::new(sb, cfg).step(req).await
        } else {
            AgyRunner::new(sb, cfg).step(req).await
        }
    }
    async fn park_and_approve(
        &self,
        mut a: Attempt,
        alias: &str,
        rx: watch::Receiver<bool>,
    ) -> Attempt {
        let res = self.step(a.clone(), alias, Some(rx)).await;
        assert_eq!(res, Err(ModelError::Cancelled));
        let seen = self.park.seen();
        assert!(seen.contains(&(a.clone(), ParkReason::Approval)));
        let ids = self.ledger.approval_ids(&a.run.0);
        assert_eq!(ids.len(), 1);
        let (id, alice) = (ids[0].clone(), PrincipalId(ALICE.into()));
        let decided = self.broker.decide(id, alice, true).await;
        decided.unwrap();
        a.claim = Some(self.ledger.bump_claim(&a.run.0));
        a
    }
    fn executed(&self) -> Vec<String> {
        let jobs = self.runner.jobs();
        jobs.into_iter().map(|j| j.tool.id.0).collect()
    }
}

fn env_var<'a>(spec: &'a ModelSpec, key: &str) -> &'a str {
    spec.env.iter().find(|(k, _)| k == key).unwrap().1.as_str()
}
fn write_rollout(spec: &ModelSpec, thread: &str, lines: &[&str]) {
    let rel = format!("codex/sessions/2026/05/01/rollout-2026-05-01T00-00-00-{thread}.jsonl");
    let p = spec.scratch.as_ref().unwrap().host.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, lines.join("\n") + "\n").unwrap();
}
async fn post(url: &str, h: &str, v: &str, body: &str) -> (u16, String) {
    let c = Client::builder().no_proxy().build().unwrap();
    let req = c
        .post(url)
        .header(h, v)
        .header("content-type", "application/json");
    let r = req.body(body.to_owned()).send().await.unwrap();
    (r.status().as_u16(), r.text().await.unwrap())
}
async fn mcp_call(base: &str, tok: &str, params: &str, is_err: bool, needle: &str) {
    let body = format!(r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{params}}}"#);
    let (url, auth) = (format!("{base}/mcp"), format!("Bearer {tok}"));
    let (code, text) = post(&url, "authorization", &auth, &body).await;
    assert_eq!(code, 200);
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["result"]["isError"], Value::Bool(is_err));
    let txt = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(txt.contains(needle), "{txt}");
}
fn assert_final(step: ModelStep, want: &str, usage: Option<(u64, u64)>) {
    let u = usage.map(|(prompt, completion)| TokenUsage { prompt, completion });
    let ok = matches!(step, ModelStep::Final { ref text, outcome, .. } if text == want && outcome.usage == u);
    assert!(ok, "{step:?}");
}
fn has_keys(keys: &[String], want: &[&str]) -> bool {
    want.iter().all(|w| keys.iter().any(|k| k == w))
}
fn has_hdr(hdrs: &[(String, String)], key: &str, val: &str) -> bool {
    hdrs.iter()
        .any(|(k, v)| k.eq_ignore_ascii_case(key) && v == val)
}
fn codex_tool_sse(id: &str, name: &str, args: &str) -> Vec<u8> {
    let it = format!(
        r#"{{"type":"function_call","call_id":"{id}","name":"{name}","arguments":"{args}"}}"#
    );
    format!("event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_1\"}}}}\n\nevent: response.output_item.added\ndata: {{\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{{\"type\":\"function_call\",\"call_id\":\"{id}\",\"name\":\"{name}\",\"arguments\":\"\"}}}}\n\nevent: response.function_call_arguments.done\ndata: {{\"type\":\"response.function_call_arguments.done\",\"output_index\":0,\"arguments\":\"{args}\"}}\n\nevent: response.output_item.done\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{it}}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\",\"output\":[{it}]}}}}\n\n").into_bytes()
}
fn codex_final_sse(text: &str) -> Vec<u8> {
    format!("event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"resp_f\"}}}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_f\",\"output\":[{{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{{\"type\":\"output_text\",\"text\":\"{text}\"}}]}}]}}}}\n\n").into_bytes()
}
fn gemini_call(name: &str, args: &str) -> Vec<u8> {
    format!("data: {{\"candidates\":[{{\"content\":{{\"role\":\"model\",\"parts\":[{{\"functionCall\":{{\"name\":\"call_mcp_tool\",\"args\":{{\"ServerName\":\"relay\",\"ToolName\":\"{name}\",\"Arguments\":{args}}}}}}}]}},\"finishReason\":\"STOP\"}}]}}\n\n").into_bytes()
}
fn gemini_text(text: &str) -> Vec<u8> {
    format!("data: {{\"candidates\":[{{\"content\":{{\"role\":\"model\",\"parts\":[{{\"text\":\"{text}\"}}]}},\"finishReason\":\"STOP\"}}]}}\n\n").into_bytes()
}

#[tokio::test]
async fn codex_cli_full_cycle_approval_park_and_rollout_resume() {
    let rig = Rig::start().await;
    let base = rig.base();

    // Phase 1: fresh read-only cycle (Responses SSE -> rollout tail -> /mcp -> Final).
    rig.script_ok(r#"{"said":"hi"}"#);
    let sse = codex_tool_sse("call_say_1", "mcp__relay__echo_say", r#"{\"msg\":\"hi\"}"#);
    rig.queue_sse(&sse);
    rig.queue_sse(&codex_final_sse("codex read done"));
    let (a1, b1) = (rig.add_run("codex-read"), base.clone());
    rig.push(move |spec, tx| async move {
        assert_eq!(spec.program, "codex");
        let tok = env_var(&spec, TOKEN_ENV).to_owned();
        let (auth, url) = (format!("Bearer {tok}"), format!("{b1}/v1/responses"));
        assert_eq!(post(&url, "authorization", &auth, CODEX_REQ).await.0, 200);
        let meta = r#"{"type":"session_meta","payload":{"id":"thr-read"}}"#;
        let call = r#"{"type":"response_item","payload":{"type":"function_call","call_id":"call_say_1","name":"mcp__relay__echo_say","arguments":"{\"msg\":\"hi\"}"}}"#;
        let out = r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"call_say_1","output":"ok"}}"#;
        write_rollout(&spec, "thr-read", &[meta, call]);
        let p = r#"{"name":"echo_say","arguments":{"msg":"hi"},"_meta":{"callId":"call_say_1"}}"#;
        mcp_call(&b1, &tok, p, false, r#""said":"hi""#).await;
        write_rollout(&spec, "thr-read", &[meta, call, out]);
        assert_eq!(post(&url, "authorization", &auth, CODEX_REQ).await.0, 200);
        let _ = tx.send(r#"{"type":"item.completed","item":{"type":"agent_message","text":"codex read done"}}"#.into());
        let _ = tx.send(r#"{"type":"turn.completed","usage":{"input_tokens":40,"output_tokens":15}}"#.into());
    });
    let s1 = rig.step(a1, "codex", None).await.unwrap();
    assert_final(s1, "codex read done", Some((40, 15)));
    let (_, path, hdrs, body) = &rig.upstream.requests()[0];
    assert_eq!(path, "/v1/responses");
    let want_auth = format!("Bearer {OPENAI_KEY}");
    assert!(has_hdr(hdrs, "authorization", &want_auth));
    let j: Value = serde_json::from_str(body).unwrap();
    assert_eq!(j["model"], "gpt-5.4");
    assert_eq!(j["store"], false);
    let rk = format!("{RESPONSE_PREFIX}{}", codex_response_id("call_say_1"));
    let keys = rig.ledger.step_keys("codex-read");
    assert!(has_keys(&keys, &[&rk, "cli-use-call_say_1", "cli-group-1"]));

    // Phase 2: irreversible tool parks on approval, revokes session, and resumes via restored rollout.
    let ds = codex_tool_sse(
        "call_drop_1",
        "mcp__relay__echo_drop",
        r#"{\"target\":\"db\"}"#,
    );
    rig.queue_sse(&ds);
    let p1 = rig.add_run("codex-park");
    let (b2, (ctx, crx)) = (base.clone(), watch::channel(false));
    rig.push(move |spec, _tx| async move {
        let tok = env_var(&spec, TOKEN_ENV).to_owned();
        let (auth, url) = (format!("Bearer {tok}"), format!("{b2}/v1/responses"));
        assert_eq!(post(&url, "authorization", &auth, CODEX_REQ).await.0, 200);
        let meta = r#"{"type":"session_meta","payload":{"id":"thr-park"}}"#;
        let call = r#"{"type":"response_item","payload":{"type":"function_call","call_id":"call_drop_1","name":"mcp__relay__echo_drop","arguments":"{\"target\":\"db\"}"}}"#;
        write_rollout(&spec, "thr-park", &[meta, call]);
        let p = r#"{"name":"echo_drop","arguments":{"target":"db"},"_meta":{"callId":"call_drop_1"}}"#;
        mcp_call(&b2, &tok, p, true, "approval_pending").await;
        assert_eq!(post(&url, "authorization", &auth, CODEX_REQ).await.0, 401);
        let _ = ctx.send(true);
        std::future::pending::<()>().await;
    });
    let p2 = rig.park_and_approve(p1, "codex", crx).await;
    rig.script_ok(r#"{"result":"dropped"}"#);
    rig.queue_sse(&codex_final_sse("codex resumed done"));
    rig.push(move |spec, tx| async move {
        assert_eq!(&spec.args[..3], &["exec", "resume", "--all"]);
        assert!(spec.args.iter().any(|a| a == "thr-park") && spec.stdin.is_none());
        let rel = "codex/sessions/2026/01/01/rollout-2026-01-01T00-00-00-thr-park.jsonl";
        let text = fs::read_to_string(spec.scratch.as_ref().unwrap().host.join(rel)).unwrap();
        assert!(text.contains("function_call_output") && text.contains("call_drop_1") && text.contains("dropped"));
        let auth = format!("Bearer {}", env_var(&spec, TOKEN_ENV));
        let url = format!("{base}/v1/responses");
        assert_eq!(post(&url, "authorization", &auth, CODEX_REQ).await.0, 200);
        let _ = tx.send(r#"{"type":"item.completed","item":{"type":"agent_message","text":"codex resumed done"}}"#.into());
        let _ = tx.send(r#"{"type":"turn.completed","usage":{"input_tokens":55,"output_tokens":20}}"#.into());
    });
    let s2 = rig.step(p2, "codex", None).await.unwrap();
    assert_final(s2, "codex resumed done", None);
    assert_eq!(rig.executed(), vec!["echo.say", "echo.drop"]);
    rig.front.stop().await;
}

#[tokio::test]
async fn agy_cli_full_cycle_approval_park_and_ledger_resume() {
    let rig = Rig::start().await;
    let base = rig.base();

    // Phase 1: fresh read-only cycle verifying zero-token template, symlink, direct commit, and /mcp.
    rig.script_ok(r#"{"said":"hi"}"#);
    rig.queue_sse(&gemini_call("echo_say", r#"{"msg":"hi"}"#));
    rig.queue_sse(&gemini_text("agy read done"));
    let a1 = rig.add_run("agy-read");
    let (l1, b1) = (rig.ledger.clone(), base.clone());
    rig.push(move |spec, tx| async move {
        assert_eq!(spec.program, "agy");
        let tok = env_var(&spec, TOKEN_ENV).to_owned();
        assert_eq!(env_var(&spec, "GEMINI_API_KEY"), tok);
        let work = &spec.scratch.as_ref().unwrap().host;
        let tpl = fs::read_to_string(work.join("mcp.agy.template.json")).unwrap();
        assert!(tpl.contains("${RELAY_TOKEN}") && !tpl.contains(&tok));
        let link = fs::read_link(work.join("home/.gemini/config/mcp_config.json")).unwrap();
        assert_eq!(link, Path::new("/tmp/mcp_config.json"));
        let msg: Value = serde_json::from_slice(spec.stdin.as_deref().unwrap()).unwrap();
        assert_eq!(msg["event"], "user");
        let url = format!("{b1}/v1beta/models/sandbox-ignored:streamGenerateContent?alt=sse");
        assert_eq!(post(&url, "x-goog-api-key", &tok, GEMINI_REQ).await.0, 200);
        let keys = l1.step_keys("agy-read");
        assert!(has_keys(&keys, &["cli-group-0", "cli-use-agy-0-0"]));
        let p = r#"{"name":"echo_say","arguments":{"msg":"hi"},"_meta":{"progressToken":"agy-read-1"}}"#;
        mcp_call(&b1, &tok, p, false, r#""said":"hi""#).await;
        assert_eq!(post(&url, "x-goog-api-key", &tok, GEMINI_REQ).await.0, 200);
        let _ = tx.send(r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"DONE","text_delta":"agy read done","usage":{"input_tokens":30,"output_tokens":12}}}"#.into());
        let _ = tx.send(r#"{"event":"result","result":{"status":"SUCCESS","response":"agy read done","usage":{"input_tokens":30,"output_tokens":12}}}"#.into());
    });
    let s1 = rig.step(a1, "agy", None).await.unwrap();
    assert_final(s1, "agy read done", Some((30, 12)));
    let (_, path, hdrs, _) = &rig.upstream.requests()[0];
    assert_eq!(path, GEMINI_PATH);
    assert!(has_hdr(hdrs, "x-goog-api-key", GEMINI_KEY));

    // Phase 2: irreversible tool parks, revokes session, and resumes via ledger-injected Exchange.
    rig.queue_sse(&gemini_call("echo_drop", r#"{"target":"db"}"#));
    let p1 = rig.add_run("agy-park");
    let (b2, (ctx, crx)) = (base.clone(), watch::channel(false));
    rig.push(move |spec, _tx| async move {
        let tok = env_var(&spec, TOKEN_ENV).to_owned();
        let url = format!("{b2}{GEMINI_PATH}");
        assert_eq!(post(&url, "x-goog-api-key", &tok, GEMINI_REQ).await.0, 200);
        let p = r#"{"name":"echo_drop","arguments":{"target":"db"},"_meta":{"progressToken":"agy-drop-1"}}"#;
        mcp_call(&b2, &tok, p, true, "approval_pending").await;
        assert_eq!(post(&url, "x-goog-api-key", &tok, GEMINI_REQ).await.0, 401);
        let _ = ctx.send(true);
        std::future::pending::<()>().await;
    });
    let p2 = rig.park_and_approve(p1, "agy", crx).await;
    rig.script_ok(r#"{"result":"dropped"}"#);
    rig.script_ok(r#"{"said":"after"}"#);
    rig.queue_sse(&gemini_call("echo_say", r#"{"msg":"after"}"#));
    rig.queue_sse(&gemini_text("agy resumed done"));
    let l2 = rig.ledger.clone();
    rig.push(move |spec, tx| async move {
        let msg: Value = serde_json::from_slice(spec.stdin.as_deref().unwrap()).unwrap();
        let content = msg["message"]["content"].as_str().unwrap();
        assert!(content.contains("# Completed steps in this request") && content.contains("`echo.drop`") && content.contains(r#"{"result":"dropped"}"#));
        let tok = env_var(&spec, TOKEN_ENV).to_owned();
        let url = format!("{base}{GEMINI_PATH}");
        assert_eq!(post(&url, "x-goog-api-key", &tok, GEMINI_REQ).await.0, 200);
        let keys = l2.step_keys("agy-park");
        assert!(has_keys(&keys, &["cli-group-0", "cli-group-1", "cli-use-agy-1-0"]));
        let p = r#"{"name":"echo_say","arguments":{"msg":"after"},"_meta":{"progressToken":"agy-after-1"}}"#;
        mcp_call(&base, &tok, p, false, r#""said":"after""#).await;
        assert_eq!(post(&url, "x-goog-api-key", &tok, GEMINI_REQ).await.0, 200);
        let _ = tx.send(r#"{"event":"result","result":{"status":"SUCCESS","response":"agy resumed done","usage":{"input_tokens":60,"output_tokens":25}}}"#.into());
    });
    let s2 = rig.step(p2, "agy", None).await.unwrap();
    assert_final(s2, "agy resumed done", None);
    assert_eq!(rig.executed(), vec!["echo.say", "echo.drop", "echo.say"]);
    rig.front.stop().await;
}

#[tokio::test]
async fn cross_protocol_isolation_rejects_mismatched_routes() {
    let rig = Rig::start().await;
    let c_run = rig.add_run("iso-c");
    let a_run = rig.add_run("iso-a");
    let c_auth = format!(
        "Bearer {}",
        rig.issue(&c_run, "codex", Protocol::OpenAiResponses).token
    );
    let a_tok = rig.issue(&a_run, "agy", Protocol::Gemini).token;
    let m = r#"{"model":"claude","messages":[]}"#;
    let g = r#"{"contents":[]}"#;
    let r = r#"{"model":"gpt-5.4","input":[]}"#;
    let gp = "/v1beta/models/x:streamGenerateContent";
    for (p, h, v, b) in [
        ("/v1/messages", "authorization", c_auth.as_str(), m),
        (gp, "authorization", c_auth.as_str(), g),
        ("/v1/messages", "x-goog-api-key", a_tok.as_str(), m),
        ("/v1/responses", "x-goog-api-key", a_tok.as_str(), r),
    ] {
        let url = format!("{}{p}", rig.base());
        assert_eq!(post(&url, h, v, b).await.0, 404, "{p}");
    }
    assert!(rig.upstream.requests().is_empty());
    rig.front.stop().await;
}
