//! Claude Code 실행기 시험: 실제 CLI 출력에서 줄인 표본으로 파서를, 가짜 샌드박스로 명세 조립과 정리를 본다.

use super::*;
use crate::model::{
    FakeSandbox, Limits, Message, ModelStep, RelayChannel, Role, RunnerKind, Script,
    sandbox::files_holding,
};
use bot_core::{id::*, profile::Builtin};
use std::{collections::BTreeSet, path::PathBuf, sync::Mutex, time::Duration};
use tokio::sync::watch;

/// 호출 폴더가 있을 때 읽은 (입력 폴더, AGENTS.md, MCP 설정)
type Seen = Arc<Mutex<Vec<(String, Option<String>, Option<String>)>>>;

/// 시작 순간 훑은 결과: (토큰이 든 파일, 설정이 쓰기 공간에 있는가, 입력 폴더에 설정이 있는가)
type Found = Arc<Mutex<Vec<(Vec<PathBuf>, bool, bool)>>>;

fn model() -> CliModel {
    CliModel {
        id: "test-model".into(),
        effort: "high".into(),
        max_output_tokens: 777,
        retries: 3,
        context_window: 1000,
    }
}

// 실제 `claude -p --output-format stream-json --include-partial-messages` 출력에서 줄인 표본
const OK: &str = r#"{"type":"system","subtype":"ui_invalidate","session_id":"s"}
{"type":"system","subtype":"init","cwd":"/w","session_id":"s","tools":[],"mcp_servers":[],"model":"claude-sonnet-5-5","apiKeySource":"ANTHROPIC_API_KEY"}
{"type":"stream_event","event":{"type":"message_start","message":{"role":"assistant"}}}
{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"흠"}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"x"}}}
{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}}
{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"1+1은 2입니"}}}
{"type":"assistant","message":{"model":"claude-sonnet-5-5","role":"assistant","content":[{"type":"text","text":"1+1은 2입니다."}]}}
{"type":"result","subtype":"success","is_error":false,"api_error_status":null,"result":"1+1은 2입니다.\n","usage":{"input_tokens":164,"output_tokens":12},"modelUsage":{"claude-sonnet-5-5":{"inputTokens":164,"outputTokens":12,"contextWindow":1000000}}}"#;

const API_ERROR: &str = r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":2,"retry_delay_ms":578,"error_status":503,"error":"server_error"}
{"type":"system","subtype":"api_retry","attempt":2,"max_retries":2,"retry_delay_ms":1139,"error_status":503,"error":"server_error"}
{"type":"result","subtype":"success","is_error":true,"api_error_status":503,"result":"API Error: 503 Service temporarily unavailable. This is a server-side issue.","usage":{"input_tokens":0,"output_tokens":0}}"#;

fn feed_all(sample: &str) -> (Vec<Event>, Result<Output, ModelError>) {
    let mut t = Tracker::default();
    let events = sample.lines().filter_map(|l| t.feed(l)).collect();
    (events, Box::new(t).finish())
}

#[test]
fn parses_progress_events_text_and_usage() {
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
    assert_eq!(out.text, "1+1은 2입니다.");
    assert_eq!(
        out.usage,
        Usage {
            input_tokens: 164,
            output_tokens: 12
        }
    );
}

#[test]
fn api_error_is_a_transient_failure_and_retries_are_signalled() {
    let (events, out) = feed_all(API_ERROR);
    assert_eq!(events, [Event::Retrying, Event::Retrying]);
    assert_eq!(
        out,
        Err(ModelError::Failed {
            code: "overloaded",
            transient: true
        }),
        "503은 잠시 뒤 다시 하면 풀릴 수 있다"
    );
    let limited =
        r#"{"type":"result","is_error":true,"api_error_status":429,"result":"rate limit"}"#;
    assert_eq!(
        feed_all(limited).1,
        Err(ModelError::Failed {
            code: "rate_limited",
            transient: true
        })
    );
}

#[test]
fn auth_error_is_not_transient() {
    let line = r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":401,"result":"Invalid API key · Fix external API key"}"#;
    assert_eq!(
        feed_all(line).1,
        Err(ModelError::Failed {
            code: "model_error",
            transient: false
        })
    );
}

#[test]
fn missing_result_and_empty_answer_are_failures() {
    assert_eq!(
        feed_all(r#"{"type":"system","subtype":"init"}"#).1,
        Err(ModelError::Failed {
            code: "no_result",
            transient: false
        })
    );
    let empty = r#"{"type":"result","subtype":"success","is_error":false,"result":"  "}"#;
    assert_eq!(
        feed_all(empty).1,
        Err(ModelError::Failed {
            code: "empty_answer",
            transient: false
        })
    );
    // 해석할 수 없는 줄은 무시한다
    assert!(Tracker::default().feed("not json").is_none());
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
fn args_run_claude_bare_with_only_the_relay_server_and_no_builtin_tools_by_default() {
    let a = ClaudeCodeRunner::args(&model(), &BTreeSet::new(), &relay(), &Mode::Ephemeral).unwrap();
    assert_eq!(a[0], "-p");
    assert_eq!(a[1], prompt::INSTRUCTION);
    for flag in [
        "--bare",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--verbose",
        "--include-partial-messages",
    ] {
        assert!(a.iter().any(|x| x == flag), "{flag} 누락");
    }
    for flag in ["--plugin-dir", "--system-prompt-file", "--settings"] {
        assert!(!a.iter().any(|x| x == flag), "{flag}는 쓰지 않아요");
    }
    assert_eq!(after(&a, "--tools"), "", "내장 도구는 기본 0개");
    assert_eq!(
        (after(&a, "--mcp-config"), after(&a, "--allowedTools")),
        ("/work/mcp.claude.json", "mcp__relay")
    );
    assert_eq!(
        (
            after(&a, "--model"),
            after(&a, "--effort"),
            after(&a, "--output-format")
        ),
        ("test-model", "high", "stream-json")
    );
    assert!(
        !a.iter().any(|x| x.contains("sess-token-1")),
        "토큰은 인자에 실리지 않는다"
    );
}

#[test]
fn only_read_can_be_opened_and_unsupported_builtin_tools_are_refused_before_running() {
    let args = |items: &[Builtin]| {
        ClaudeCodeRunner::args(&model(), &builtin(items), &relay(), &Mode::Ephemeral)
    };
    assert_eq!(after(&args(&[Builtin::Read]).unwrap(), "--tools"), "Read");
    let unsupported = Err(ModelError::Failed {
        code: "unsupported_builtin",
        transient: false,
    });
    for items in [
        &[Builtin::Grep][..],
        &[Builtin::Glob],
        &[Builtin::Read, Builtin::Grep],
        &[Builtin::Read, Builtin::Grep, Builtin::Glob],
    ] {
        assert_eq!(args(items), unsupported, "{items:?}");
    }
}

fn workspace(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("bot-runtime-claude-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn request(
    model: &str,
    cancel: watch::Receiver<bool>,
    relay: Option<RelayChannel>,
) -> ModelRequest {
    ModelRequest {
        attempt: Attempt {
            profile: ProfileId("personal".into()),
            run: RunId("r/1".into()),
            claim: Some(bot_core::id::ClaimEpoch(1)),
            generation: Generation(2),
        },
        model: model.into(),
        system: "너는 비서다.".into(),
        conversation: vec![Message {
            role: Role::User,
            speaker: Some(PrincipalId("local:owner".into())),
            text: "1+1은?".into(),
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
async fn step_hands_the_sandbox_a_secret_free_spec_and_returns_the_final_answer() {
    let ws = workspace("spec");
    let seen: Seen = Arc::default();
    let sink = seen.clone();
    let sandbox = FakeSandbox::new([Script::output(OK)]).inspect(move |spec| {
        // 호출 폴더가 아직 있을 때 지침과 도구 설정 파일을 읽는다
        let input = &spec.readonly_mounts[0].host;
        let work = &spec.scratch.as_ref().unwrap().host;
        let agents = std::fs::read_to_string(input.join("AGENTS.md")).ok();
        let mcp = std::fs::read_to_string(work.join(MCP_CONFIG)).ok();
        sink.lock()
            .unwrap()
            .push((input.display().to_string(), agents, mcp));
    });
    let config = CliConfig::new(&ws).with_model("assistant", model());
    let runner = ClaudeCodeRunner::new(Arc::new(sandbox.clone()), config);
    assert_eq!(
        runner.kind(),
        RunnerKind::Loop,
        "CLI는 자기 도구 순환을 가진다"
    );
    let (_tx, rx) = watch::channel(false);
    let step = runner
        .step(request("assistant", rx, Some(relay())))
        .await
        .unwrap();
    assert_eq!(
        step,
        ModelStep::Final {
            text: "1+1은 2입니다.".into(),
            usage: Usage {
                input_tokens: 164,
                output_tokens: 12
            }
        }
    );

    let spec = sandbox.specs().remove(0);
    assert_eq!(
        (spec.program.as_str(), spec.cwd.as_str()),
        ("claude", "/bot")
    );
    let env = |k: &str| {
        spec.env
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(env("ANTHROPIC_BASE_URL"), Some("http://192.168.64.1:8787"));
    assert_eq!(
        env("ANTHROPIC_API_KEY"),
        Some("sess-token-1"),
        "이 시도에 발급한 세션 토큰이다(실제 제공자 키가 아니다)"
    );
    assert_eq!(
        env("RELAY_TOKEN"),
        Some("sess-token-1"),
        "MCP 설정의 자리표시가 채워지는 곳(토큰은 환경으로만 간다)"
    );
    for (name, want) in [
        ("DISABLE_TELEMETRY", "1"),
        ("DISABLE_AUTOUPDATER", "1"),
        ("DISABLE_ERROR_REPORTING", "1"),
    ] {
        assert_eq!(env(name), Some(want), "{name}");
    }
    assert_eq!(
        (
            env("CLAUDE_CODE_MAX_OUTPUT_TOKENS"),
            env("CLAUDE_CODE_MAX_RETRIES")
        ),
        (Some("777"), Some("3"))
    );
    assert_eq!(
        (env("HOME"), env("CLAUDE_CONFIG_DIR")),
        (Some("/work/home"), Some("/work/claude"))
    );
    assert_eq!(env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"), Some("1"));
    assert!(
        spec.env.iter().all(|(k, _)| !k.starts_with("BOT_")),
        "요청자·권한은 환경으로 가지 않는다"
    );
    assert_eq!(spec.readonly_mounts.len(), 1);
    assert_eq!(spec.readonly_mounts[0].guest, "/bot");
    let scratch = spec.scratch.as_ref().unwrap();
    assert_eq!(scratch.guest, "/work");
    assert_ne!(scratch.host, spec.readonly_mounts[0].host);
    let stdin = String::from_utf8(spec.stdin.clone().unwrap()).unwrap();
    assert!(
        stdin.contains("### 사용자 (local:owner)\n1+1은?"),
        "{stdin}"
    );
    assert_eq!(
        (spec.timeout, spec.idle_timeout),
        (Duration::from_secs(5), Duration::from_secs(2))
    );
    assert_eq!(
        (spec.attempt.run.0.as_str(), spec.attempt.generation.0),
        ("r/1", 2),
        "샌드박스 구현은 이 claim의 브로커 채널만 연결한다"
    );

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].1.as_deref(), Some("너는 비서다."));
    let mcp = seen[0]
        .2
        .as_deref()
        .expect("중계가 있으면 MCP 설정 파일이 모델이 읽는 입력 폴더 밖(쓰기 공간)에 있다");
    let mcp: serde_json::Value = serde_json::from_str(mcp).unwrap();
    assert_eq!(mcp["mcpServers"]["relay"]["type"], "http");
    assert_eq!(
        mcp["mcpServers"]["relay"]["url"],
        "http://192.168.64.1:8787/mcp"
    );
    assert_eq!(
        mcp["mcpServers"]["relay"]["headers"]["Authorization"], "Bearer ${RELAY_TOKEN}",
        "토큰 값은 파일에 없고 환경변수 자리표시만 있다"
    );
    assert!(
        seen[0].0.contains("personal-r_1-g2-"),
        "폴더 이름에는 안전한 글자만: {}",
        seen[0].0
    );
    assert_eq!(
        std::fs::read_dir(&ws).unwrap().count(),
        0,
        "호출이 끝나면 호출 폴더를 지운다"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn the_session_token_is_in_the_environment_only_never_in_a_file_or_argument() {
    let ws = workspace("sweep");
    // 시작 순간(호출 폴더가 있을 때) 모델이 읽을 수 있는 두 폴더 전체를 훑는다: 입력(`/bot`)과 쓰기 공간(`/work`)
    let found: Found = Arc::default();
    let sink = found.clone();
    let sandbox = FakeSandbox::new([Script::output(OK)]).inspect(move |spec| {
        let input = &spec.readonly_mounts[0].host;
        let work = &spec.scratch.as_ref().unwrap().host;
        let hits = files_holding(&[input, work], "sess-token-1");
        sink.lock().unwrap().push((
            hits,
            work.join(MCP_CONFIG).is_file(),
            input.join(".agents").exists() || input.join(MCP_CONFIG).exists(),
        ));
    });
    let config = CliConfig::new(&ws)
        .with_model("assistant", model())
        .with_confined_reads();
    let runner = ClaudeCodeRunner::new(Arc::new(sandbox.clone()), config);
    let (_tx, rx) = watch::channel(false);
    let mut asked = request("assistant", rx, Some(relay()));
    // 내장 Read를 켠 구성: 모델이 입력 폴더를 읽을 수 있다
    asked.builtin = builtin(&[Builtin::Read]);
    runner.step(asked).await.unwrap();

    let found = found.lock().unwrap();
    assert_eq!(
        found[0].0,
        Vec::<PathBuf>::new(),
        "모델이 읽는 폴더의 어떤 파일에도 세션 토큰이 없다"
    );
    assert!(found[0].1, "설정은 쓰기 공간에 있다");
    assert!(!found[0].2, "모델이 읽는 입력 폴더에는 MCP 설정이 없다");
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
        ["ANTHROPIC_API_KEY", "RELAY_TOKEN"],
        "토큰을 싣는 환경변수는 이 둘뿐이다"
    );
    let _ = std::fs::remove_dir_all(&ws);
}

async fn run_script(config: &CliConfig, script: Script) -> Result<ModelStep, ModelError> {
    let runner = ClaudeCodeRunner::new(Arc::new(FakeSandbox::new([script])), config.clone());
    let (_tx, rx) = watch::channel(false);
    runner.step(request("assistant", rx, Some(relay()))).await
}

#[tokio::test]
async fn step_maps_failures_to_fixed_codes_and_cleans_up() {
    let ws = workspace("fail");
    let config = CliConfig::new(&ws).with_model("assistant", model());
    assert_eq!(
        run_script(&config, Script::output(API_ERROR)).await,
        Err(ModelError::Failed {
            code: "overloaded",
            transient: true
        })
    );
    // 결과 없이 죽었다: stderr는 분류에만 쓴다
    assert_eq!(
        run_script(
            &config,
            Script::output("").exits(1, "fetch failed: ECONNRESET")
        )
        .await,
        Err(ModelError::Failed {
            code: "network",
            transient: true
        })
    );
    assert_eq!(
        run_script(&config, Script::output("").exits(1, "Segmentation fault")).await,
        Err(ModelError::Failed {
            code: "no_result",
            transient: false
        })
    );
    assert_eq!(
        run_script(&config, Script::refused("no_runtime")).await,
        Err(ModelError::Failed {
            code: "sandbox_failed",
            transient: true
        })
    );
    // 모르는 별칭·잘못된 프런트 채널·채널 없음은 샌드박스를 부르기 전에 거절한다
    let sandbox = FakeSandbox::new([]);
    let runner = ClaudeCodeRunner::new(Arc::new(sandbox.clone()), config.clone());
    let (_tx, rx) = watch::channel(false);
    assert_eq!(
        runner
            .step(request("nope", rx.clone(), Some(relay())))
            .await,
        Err(ModelError::Failed {
            code: "unknown_model",
            transient: false
        })
    );
    assert_eq!(
        runner.step(request("assistant", rx.clone(), None)).await,
        Err(ModelError::Failed {
            code: "relay_unavailable",
            transient: true
        }),
        "채널이 없으면 CLI가 모델에 닿지 못한다"
    );
    let bad = RelayChannel {
        token: "has space".into(),
        ..relay()
    };
    assert_eq!(
        runner.step(request("assistant", rx, Some(bad))).await,
        Err(ModelError::Failed {
            code: "invalid_relay",
            transient: false
        })
    );
    assert!(sandbox.specs().is_empty());
    assert_eq!(std::fs::read_dir(&ws).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&ws);
}

#[tokio::test]
async fn builtin_tools_need_the_confined_reads_guarantee_and_reach_the_launch_arguments() {
    let ws = workspace("builtin");
    let run = |confined: bool, items: Vec<Builtin>| {
        let ws = ws.clone();
        async move {
            let mut config = CliConfig::new(&ws).with_model("assistant", model());
            if confined {
                config = config.with_confined_reads();
            }
            let sandbox = FakeSandbox::new([Script::output(OK)]);
            let runner = ClaudeCodeRunner::new(Arc::new(sandbox.clone()), config);
            let (_tx, rx) = watch::channel(false);
            let mut req = request("assistant", rx, Some(relay()));
            req.builtin = items.into_iter().collect();
            let result = runner.step(req).await;
            (result, sandbox.specs())
        }
    };
    // 가둘 수 있다고 보증하지 않으면 내장 도구를 요구하는 실행을 거절한다(샌드박스도 부르지 않는다)
    let (result, specs) = run(false, vec![Builtin::Read]).await;
    assert_eq!(
        result,
        Err(ModelError::Failed {
            code: "builtin_unconfined",
            transient: false
        })
    );
    assert!(specs.is_empty());
    // 보증이 있으면 명시한 것만 연다
    let (result, specs) = run(true, vec![Builtin::Read]).await;
    assert!(result.is_ok());
    assert_eq!(after(&specs[0].args, "--tools"), "Read");
    // 내장 도구를 요구하지 않으면 보증이 없어도 실행되고 도구는 0개다
    let (result, specs) = run(false, vec![]).await;
    assert!(result.is_ok());
    assert_eq!(after(&specs[0].args, "--tools"), "");
    // 실행기가 못 여는 도구는 실행 전에 거절한다
    let (result, specs) = run(true, vec![Builtin::Glob]).await;
    assert_eq!(
        result,
        Err(ModelError::Failed {
            code: "unsupported_builtin",
            transient: false
        })
    );
    assert!(specs.is_empty());
    assert_eq!(std::fs::read_dir(&ws).unwrap().count(), 0);
    let _ = std::fs::remove_dir_all(&ws);
}
