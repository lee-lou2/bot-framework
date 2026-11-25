//! Pi 실행기 시험: v1에서 검증한 표본 출력으로 파서를, 가짜 샌드박스로 설정 파일·명세 조립을 본다.

use super::*;
use crate::model::{
    FakeSandbox, Limits, Message, ModelStep, RelayChannel, Role, RunnerKind, Script,
    sandbox::files_holding,
};
use bot_core::{id::*, profile::Builtin};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};
use tokio::sync::watch;

/// 시작 순간 훑은 결과: (토큰이 든 파일, MCP 설정이 있는가)
type Found = Arc<Mutex<Vec<(Vec<PathBuf>, bool)>>>;

fn gpt() -> CliModel {
    CliModel {
        id: "gpt-6.1-sol".into(),
        effort: "max".into(),
        max_output_tokens: 777,
        retries: 3,
        context_window: 1000,
    }
}

const OK: &str = r#"{"type":"session","version":3,"id":"s","timestamp":"2026-11-11T00:00:00Z","cwd":"/bot"}
{"type":"agent_start"}
{"type":"turn_start"}
{"type":"message_update","usage":{"input":10,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":12},"assistantMessageEvent":{"type":"thinking_start","contentIndex":0}}
{"type":"message_update","usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15},"assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"생각"}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"확인해 볼게요"},{"type":"toolCall","id":"c1","name":"read","arguments":{"path":"a"}}],"stopReason":"toolUse","usage":{"input":100,"output":20,"cacheRead":50,"cacheWrite":0,"totalTokens":170}}}
{"type":"turn_end","message":{"role":"assistant"},"toolResults":[]}
{"type":"turn_start"}
{"type":"message_update","usage":{"input":10,"output":2,"cacheRead":0,"cacheWrite":0,"totalTokens":12},"assistantMessageEvent":{"type":"text_start","contentIndex":0}}
{"type":"message_update","usage":{"input":10,"output":8,"cacheRead":0,"cacheWrite":0,"totalTokens":18},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"**비서**예요."}}
{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"**비서**예요.\n"}],"stopReason":"stop","usage":{"input":80,"output":12,"cacheRead":120,"cacheWrite":0,"totalTokens":212}}}
{"type":"agent_end","messages":[],"willRetry":false}
{"type":"agent_settled","aborted":false}"#;

const API_ERROR: &str = r#"{"type":"session","version":3,"id":"s","timestamp":"2026-11-11T00:00:00Z","cwd":"/bot"}
{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"503 Service temporarily unavailable","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0}}}
{"type":"auto_retry_start","attempt":1,"maxAttempts":1,"delayMs":2000,"errorMessage":"503 Service temporarily unavailable"}
{"type":"auto_retry_end","success":false,"attempt":1,"finalError":"503 Service temporarily unavailable"}
{"type":"agent_settled","aborted":false}"#;

fn feed_all(sample: &str) -> (Vec<Event>, Result<Output, ModelError>) {
    let mut t = Tracker::default();
    let events = sample.lines().filter_map(|l| t.feed(l)).collect();
    (events, Box::new(t).finish())
}

#[test]
fn parses_progress_events_last_answer_and_total_usage() {
    let (events, out) = feed_all(OK);
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
    assert_eq!(
        out.text, "**비서**예요.",
        "도구 호출 전 중간 글이 아니라 마지막 답만 쓴다"
    );
    assert_eq!(
        out.usage,
        Usage {
            input_tokens: 100 + 50 + 80 + 120,
            output_tokens: 20 + 12
        }
    );
}

#[test]
fn api_error_is_transient_when_status_is_503_and_permanent_on_401() {
    let (events, out) = feed_all(API_ERROR);
    assert_eq!(events, [Event::Retrying]);
    assert_eq!(
        out,
        Err(ModelError::Failed {
            code: "overloaded",
            transient: true
        })
    );
    let auth_err = r#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"401 Unauthorized: Invalid API key","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0}}}
{"type":"agent_settled","aborted":false}"#;
    assert_eq!(
        feed_all(auth_err).1,
        Err(ModelError::Failed {
            code: "model_error",
            transient: false
        })
    );
}

#[test]
fn unsettled_aborted_or_empty_run_is_a_failure() {
    assert_eq!(
        feed_all(r#"{"type":"agent_start"}"#).1,
        Err(ModelError::Failed {
            code: "no_result",
            transient: false
        })
    );
    let empty = r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"  "}],"stopReason":"stop","usage":{"input":1,"output":1}}}
{"type":"agent_settled","aborted":false}"#;
    assert_eq!(
        feed_all(empty).1,
        Err(ModelError::Failed {
            code: "empty_answer",
            transient: false
        })
    );
    let aborted = r#"{"type":"agent_settled","aborted":true}"#;
    assert_eq!(
        feed_all(aborted).1,
        Err(ModelError::Failed {
            code: "model_error",
            transient: false
        })
    );
}

fn relay() -> RelayChannel {
    RelayChannel {
        name: "relay".into(),
        base_url: "http://192.168.64.1:8787".into(),
        token: "sess-token-1".into(),
    }
}

fn builtin(items: &[Builtin]) -> BTreeSet<Builtin> {
    items.iter().copied().collect()
}

fn after<'a>(a: &'a [String], flag: &str) -> &'a str {
    a[a.iter()
        .position(|x| x == flag)
        .unwrap_or_else(|| panic!("{flag} 없음"))
        + 1]
    .as_str()
}

#[test]
fn args_open_only_the_declared_read_only_tools_plus_tool_search() {
    let none = PiRunner::args(&gpt(), "acme", &BTreeSet::new());
    assert_eq!(
        &none[..5],
        ["--mode", "json", "--no-session", "--approve", "-ne"]
    );
    assert_eq!(
        after(&none, "--tools"),
        "tool_search",
        "내장 도구는 기본 0개(지연 도구 검색만)"
    );
    assert!(
        none.iter().all(|a| a != "--no-mcp"),
        "프런트가 필수이므로 MCP는 항상 켠다"
    );
    let all = PiRunner::args(
        &gpt(),
        "acme",
        &builtin(&[Builtin::Read, Builtin::Grep, Builtin::Glob]),
    );
    assert_eq!(
        after(&all, "--tools"),
        "read,grep,find,tool_search",
        "Read·Grep·Glob → read·grep·find, ls·bash·edit·write는 열지 않는다"
    );
    let grep_only = PiRunner::args(&gpt(), "acme", &builtin(&[Builtin::Grep]));
    assert_eq!(after(&grep_only, "--tools"), "grep,tool_search");
    assert_eq!(
        (
            after(&all, "--provider"),
            after(&all, "--model"),
            after(&all, "--thinking")
        ),
        ("acme", "gpt-6.1-sol", "max")
    );
    assert_eq!(after(&all, "--"), prompt::INSTRUCTION);
}

#[test]
fn models_config_picks_the_protocol_by_model_name() {
    let openai: Value =
        serde_json::from_str(&models_config(&gpt(), "acme", "http://relay.invalid")).unwrap();
    let p = &openai["providers"]["acme"];
    assert_eq!(
        (
            p["api"].as_str(),
            p["baseUrl"].as_str(),
            p["apiKey"].as_str()
        ),
        (
            Some("openai-responses"),
            Some("http://relay.invalid/v1"),
            Some("$PI_API_KEY")
        )
    );
    let model = &p["models"][0];
    assert_eq!(
        (
            model["id"].as_str(),
            model["reasoning"].as_bool(),
            model["contextWindow"].as_u64(),
            model["maxTokens"].as_u64()
        ),
        (Some("gpt-6.1-sol"), Some(true), Some(1000), Some(777))
    );
    // `--thinking`은 모델 능력에 맞춰 잘린다 — 모델의 세기가 요청까지 가려면 low~max가 모두 대응돼 있어야 한다
    for level in ["low", "medium", "high", "xhigh", "max"] {
        assert_eq!(model["thinkingLevelMap"][level], level, "{level}");
    }
    assert!(
        model["thinkingLevelMap"]["off"].is_null()
            && model["thinkingLevelMap"]["minimal"].is_null()
    );

    let claude = CliModel {
        id: "claude-sonnet-5-5".into(),
        ..gpt()
    };
    let claude: Value =
        serde_json::from_str(&models_config(&claude, "acme", "http://relay.invalid")).unwrap();
    assert_eq!(
        (
            claude["providers"]["acme"]["api"].as_str(),
            claude["providers"]["acme"]["baseUrl"].as_str()
        ),
        (Some("anthropic-messages"), Some("http://relay.invalid"))
    );
}

fn workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bot-runtime-pi-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn request(cancel: watch::Receiver<bool>, relay: Option<RelayChannel>) -> ModelRequest {
    ModelRequest {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(1),
        },
        model: "assistant".into(),
        system: "너는 비서다.".into(),
        conversation: vec![Message {
            role: Role::User,
            speaker: None,
            text: "누구야?".into(),
        }],
        tools: vec![],
        exchanges: vec![],
        limits: Limits {
            total: Duration::from_secs(5),
            idle: Duration::from_secs(2),
        },
        cancel,
        builtin: BTreeSet::new(),
        relay,
        session: None,
    }
}

#[tokio::test]
async fn the_session_token_is_in_the_environment_only_never_in_a_file_or_argument() {
    let ws = workspace("sweep");
    // 시작 순간 모델이 읽을 수 있는 두 폴더 전체를 훑는다: 입력(`/bot`)과 쓰기 공간(`/work`, Pi 설정 폴더 포함)
    let found: Found = Arc::default();
    let sink = found.clone();
    let sandbox = FakeSandbox::new([Script::output(OK)]).inspect(move |spec| {
        let input = &spec.readonly_mounts[0].host;
        let work = &spec.scratch.as_ref().unwrap().host;
        sink.lock().unwrap().push((
            files_holding(&[input, work], "sess-token-1"),
            work.join(AGENT_DIR).join("mcp.json").is_file(),
        ));
    });
    let mut config = CliConfig::new(&ws).with_model("assistant", gpt());
    config.pi_provider = "acme".into();
    let runner = PiRunner::new(Arc::new(sandbox.clone()), config);
    let (_tx, rx) = watch::channel(false);
    runner.step(request(rx, Some(relay()))).await.unwrap();

    let found = found.lock().unwrap();
    assert_eq!(
        found[0].0,
        Vec::<PathBuf>::new(),
        "모델이 읽는 폴더의 어떤 파일에도 세션 토큰이 없다"
    );
    assert!(found[0].1, "MCP 설정은 있다(자리표시만)");
    let spec = sandbox.specs().remove(0);
    assert!(
        spec.args.iter().all(|a| !a.contains("sess-token-1")),
        "{:?}",
        spec.args
    );
    let carrying: Vec<&str> = spec
        .env
        .iter()
        .filter(|(_, v)| v.contains("sess-token-1"))
        .map(|(k, _)| k.as_str())
        .collect();
    assert_eq!(
        carrying,
        ["PI_API_KEY", "RELAY_TOKEN"],
        "토큰을 싣는 환경변수는 이 둘뿐이다"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn step_writes_isolated_agent_config_and_passes_no_secret() {
    let ws = workspace("spec");
    let seen: Arc<Mutex<BTreeMap<String, String>>> = Arc::default();
    let sink = seen.clone();
    let sandbox = FakeSandbox::new([Script::output(OK)]).inspect(move |spec| {
        let work = &spec.scratch.as_ref().unwrap().host;
        let mut files = sink.lock().unwrap();
        for name in ["models.json", "settings.json", "mcp.json"] {
            if let Ok(text) = std::fs::read_to_string(work.join(AGENT_DIR).join(name)) {
                files.insert(name.to_string(), text);
            }
        }
        if let Ok(text) = std::fs::read_to_string(spec.readonly_mounts[0].host.join("AGENTS.md")) {
            files.insert("AGENTS.md".into(), text);
        }
    });
    let mut config = CliConfig::new(&ws).with_model("assistant", gpt());
    config.pi_provider = "acme".into();
    let runner = PiRunner::new(Arc::new(sandbox.clone()), config);
    assert_eq!(
        runner.kind(),
        RunnerKind::Loop,
        "CLI는 자기 도구 순환을 가진다"
    );
    let (_tx, rx) = watch::channel(false);
    let step = runner.step(request(rx, Some(relay()))).await.unwrap();
    assert_eq!(
        step,
        ModelStep::Final {
            text: "**비서**예요.".into(),
            usage: Usage {
                input_tokens: 350,
                output_tokens: 32
            },
            outcome: Default::default(),
        }
    );

    let spec = sandbox.specs().remove(0);
    assert_eq!((spec.program.as_str(), spec.cwd.as_str()), ("pi", "/bot"));
    let env = |k: &str| {
        spec.env
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        (env("HOME"), env("PI_CODING_AGENT_DIR")),
        (Some("/work/pi-home"), Some("/work/pi-agent"))
    );
    assert_eq!(
        (
            env("PI_OFFLINE"),
            env("PI_SKIP_VERSION_CHECK"),
            env("PI_TELEMETRY")
        ),
        (Some("1"), Some("1"), Some("0"))
    );
    assert_eq!(
        env("PI_API_KEY"),
        Some("sess-token-1"),
        "이 시도에 발급한 세션 토큰이다(실제 제공자 키가 아니다)"
    );
    assert!(spec.env.iter().all(|(k, _)| !k.starts_with("BOT_")));
    assert!(
        spec.args.iter().all(|a| !a.contains("sess-token-1")),
        "토큰이 인자에 실리지 않는다"
    );

    let files = seen.lock().unwrap();
    let models: Value = serde_json::from_str(&files["models.json"]).unwrap();
    assert_eq!(models["providers"]["acme"]["api"], "openai-responses");
    assert_eq!(
        models["providers"]["acme"]["baseUrl"], "http://192.168.64.1:8787/v1",
        "모델 요청도 같은 프런트로 간다"
    );
    let settings: Value = serde_json::from_str(&files["settings.json"]).unwrap();
    assert_eq!(
        (
            settings["retry"]["enabled"].as_bool(),
            settings["retry"]["maxRetries"].as_u64()
        ),
        (Some(true), Some(3))
    );
    let mcp: Value = serde_json::from_str(&files["mcp.json"]).unwrap();
    assert_eq!(
        mcp["mcpServers"]["relay"]["url"],
        "http://192.168.64.1:8787/mcp"
    );
    assert_eq!(
        mcp["mcpServers"]["relay"]["headers"]["Authorization"], "Bearer ${RELAY_TOKEN}",
        "토큰 값은 파일에 없고 환경변수 자리표시만 있다"
    );
    assert_eq!(mcp["mcpServers"]["relay"]["exposure"], "deferred");
    assert_eq!(
        (spec.attempt.run.0.as_str(), spec.attempt.generation.0),
        ("r1", 1)
    );
    assert_eq!(files["AGENTS.md"], "너는 비서다.");
    assert_eq!(
        std::fs::read_dir(&ws).unwrap().count(),
        0,
        "호출이 끝나면 호출 폴더를 지운다"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn step_reports_transient_failures_with_a_fixed_code() {
    let ws = workspace("fail");
    let config = CliConfig::new(&ws).with_model("assistant", gpt());
    let runner = PiRunner::new(
        Arc::new(FakeSandbox::new([Script::output(API_ERROR)])),
        config,
    );
    let (_tx, rx) = watch::channel(false);
    let error = runner.step(request(rx, Some(relay()))).await.unwrap_err();
    assert_eq!(
        error,
        ModelError::Failed {
            code: "overloaded",
            transient: true
        }
    );
    assert_eq!(
        error.failure(),
        bot_core::command::Failure::Transient("overloaded")
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn without_a_front_channel_or_confined_reads_pi_does_not_start() {
    let ws = workspace("guards");
    let sandbox = FakeSandbox::new([]);
    let config = CliConfig::new(&ws).with_model("assistant", gpt());
    let runner = PiRunner::new(Arc::new(sandbox.clone()), config.clone());
    let (_tx, rx) = watch::channel(false);
    assert_eq!(
        runner.step(request(rx.clone(), None)).await,
        Err(ModelError::Failed {
            code: "relay_unavailable",
            transient: true
        })
    );
    let mut with_read = request(rx.clone(), Some(relay()));
    with_read.builtin = builtin(&[Builtin::Read]);
    assert_eq!(
        runner.step(with_read.clone()).await,
        Err(ModelError::Failed {
            code: "builtin_unconfined",
            transient: false
        }),
        "읽기를 가둔다는 보증이 없으면 내장 도구 실행을 거절한다"
    );
    assert!(sandbox.specs().is_empty());
    // 보증이 있으면 선언한 도구가 실행 인자에 그대로 실린다
    let sandbox = FakeSandbox::new([Script::output(OK)]);
    let runner = PiRunner::new(Arc::new(sandbox.clone()), config.with_confined_reads());
    let mut all = with_read;
    all.builtin = builtin(&[Builtin::Read, Builtin::Grep, Builtin::Glob]);
    assert!(runner.step(all).await.is_ok());
    assert_eq!(
        after(&sandbox.specs()[0].args, "--tools"),
        "read,grep,find,tool_search"
    );
    let _ = std::fs::remove_dir_all(&ws);
}
