//! Claude Code CLI(`claude -p`) 실행기.
//!
//! 루프 내장(`RunnerKind::Loop`) 실행기다: CLI가 자기 도구 순환을 돌리고 워커는 차례를 제어하지 않는다. 항상 `ModelStep::Final`을 돌려준다.
//! 샌드박스(VM) 안에서 `/bot`(읽기 전용 입력 폴더)을 작업 폴더로 `--bare`로 실행한다. 지침은 입력 폴더의 `AGENTS.md`에 두고 `--append-system-prompt-file`로 시스템 지침에
//! 실어 준다(실측: `--bare`는 작업 폴더의 `AGENTS.md`·`CLAUDE.md`를 자동으로 읽지 않으므로 이 인자가 없으면 지침이 모델에 닿지 않는다). 대화는 표준입력으로 준다:
//! 사용자가 쓴 글은 사용자 메시지에만 있고 시스템 지침에는 이 파일(프로필이 정한 지침·스킬·지식과 고정 안내)만 있다.
//! 모델과 도구는 `ModelRequest::relay`(run별 브로커 프런트)로만 나간다: `ANTHROPIC_BASE_URL`은 그 주소, `ANTHROPIC_API_KEY`는 이 시도에 발급한
//! 세션 토큰이다(실제 제공자 키는 VM에 없다). 같은 주소로 `--mcp-config`(HTTP MCP) 파일을 만들되 **모델이 읽는 폴더(`/bot`) 밖인 쓰기 공간(`/work`)에** 두고,
//! 토큰은 그 파일에 쓰지 않는다: 헤더에는 `${RELAY_TOKEN}` 자리표시만 있고 값은 환경변수로만 간다(그래서 내장 Read를 켜도 모델이 읽는 어떤 파일에도 토큰이 없다).
//! `--strict-mcp-config`로 그것만 쓰며 그 서버의 도구만 `--allowedTools`로 미리 허용한다(`-p`에는 승인할 사람이 없다 — 승인이 필요한 호출은 브로커가 막고 run을 멈춘다).
//! 내장 도구는 기본 0개(`--tools ""`)다. 에이전트가 `builtin`에 명시한 것만 연다: 실측으로 `--bare`에서는 `Read`만 켜지므로 `Grep`·`Glob` 요구는
//! 실행 전에 `unsupported_builtin`으로 거절한다(다른 도구로 바꿔 열지 않는다).
//!
//! 세션: 호스트가 세션 포트(`ModelRequest::session`)를 주면 run마다 정해진 세션 ID로 세션을 남기고(`--session-id`), 승인·위임으로 run이 멈췄다
//! 다시 깨어나면 체크포인트에서 세션을 되살려 `--resume`으로 잇는다(`session` 모듈). 포트가 없으면 세션을 남기지 않는다(`--no-session-persistence`).
//! 성공·실패는 마지막 `result` 이벤트의 `is_error`로 알고(API 오류면 종료 코드도 1), 진행은 `stream_event`·`api_retry`로 안다.

use super::{
    CliConfig, CliModel, ModelError, ModelRequest, ModelRunner, RelayChannel, RunnerKind,
    StepFuture, TOKEN_ENV, ToolOutput, Usage,
    cli::{self, BOT_MOUNT, CallDir, WORK_MOUNT},
    driver::{self, Event, Output, Parser, classify},
    prompt,
    session::{
        Journal, Session, SessionError, Slot, Tail, project_dir, rec_step, restore, result_line,
        session_id,
    },
};
use bot_core::{
    command::{RunOutcome, TokenUsage},
    profile::Builtin,
    sandbox::{ModelSandbox, ModelSpec, Mount},
};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch;

/// 세션 파일을 따라 읽는 간격
const TAIL_EVERY: Duration = Duration::from_millis(25);

/// CLI가 세션을 어떻게 다루는가.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    /// 세션을 남기지 않는다(포트 없음)
    Ephemeral,
    /// 이 ID로 새 세션을 시작한다
    Fresh(String),
    /// 체크포인트에서 되살린 이 ID의 세션을 잇는다(원래 요청 본문은 다시 보내지 않는다)
    Resume(String),
}

/// 필수가 아닌 통신(자동 업데이트·원격 측정·오류 보고)을 끄고, 이 봇에 필요 없는 것을 뺀다: 자동 메모리(호출마다 새로 시작한다), claude 번들 스킬.
/// 공식 환경변수다: https://code.claude.com/docs/en/env-vars
const FIXED_ENV: [(&str, &str); 6] = [
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_AUTOUPDATER", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    ("CLAUDE_CODE_DISABLE_BUNDLED_SKILLS", "1"),
];

/// 도구 서버 설정 파일의 쓰기 공간(`/work`) 안 위치. 모델이 읽는 입력 폴더(`/bot`) 밖이다.
pub const MCP_CONFIG: &str = "mcp.claude.json";

pub struct ClaudeCodeRunner {
    sandbox: Arc<ModelSandbox>,
    config: CliConfig,
}

impl ClaudeCodeRunner {
    pub fn new(sandbox: Arc<ModelSandbox>, config: CliConfig) -> Self {
        Self { sandbox, config }
    }

    /// 명령 인자. 내장 도구는 명시한 것만 연다(비어 있으면 `--tools ""`). 이 실행기가 제공하지 못하는 것을 요구하면 실행 전에 거절한다.
    pub fn args(
        model: &CliModel,
        builtin: &BTreeSet<Builtin>,
        relay: &RelayChannel,
        mode: &Mode,
    ) -> Result<Vec<String>, ModelError> {
        let tools = if builtin.is_empty() {
            String::new()
        } else if builtin.iter().all(|b| *b == Builtin::Read) {
            "Read".to_owned()
        } else {
            // 실측: `--bare`에서 Grep·Glob은 켜지지 않는다. Bash 등으로 대신 열지 않는다
            return Err(cli::failed("unsupported_builtin"));
        };
        let instruction = match mode {
            Mode::Resume(_) => prompt::RESUME_INSTRUCTION,
            _ => prompt::INSTRUCTION,
        };
        let mut a: Vec<String> = [
            "-p",
            instruction,
            "--bare",
            "--model",
            &model.id,
            "--effort",
            &model.effort,
            "--append-system-prompt-file",
            &format!("{BOT_MOUNT}/AGENTS.md"),
            "--strict-mcp-config",
            "--mcp-config",
            &format!("{WORK_MOUNT}/{MCP_CONFIG}"),
            "--allowedTools",
            &relay.claude_allowed_tools(),
        ]
        .map(String::from)
        .into();
        a.extend(["--tools".to_string(), tools]);
        match mode {
            Mode::Ephemeral => a.push("--no-session-persistence".into()),
            Mode::Fresh(id) => a.extend(["--session-id".to_string(), id.clone()]),
            Mode::Resume(id) => a.extend(["--resume".to_string(), id.clone()]),
        }
        a.extend(
            [
                "--output-format",
                "stream-json",
                "--verbose",
                "--include-partial-messages",
            ]
            .map(String::from),
        );
        Ok(a)
    }

    fn env(&self, model: &CliModel, relay: &RelayChannel) -> Vec<(String, String)> {
        let mut env: Vec<(String, String)> = FIXED_ENV
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        env.extend([
            ("HOME".to_string(), format!("{WORK_MOUNT}/home")),
            (
                "CLAUDE_CONFIG_DIR".to_string(),
                format!("{WORK_MOUNT}/claude"),
            ),
            ("ANTHROPIC_BASE_URL".to_string(), relay.base_url.clone()),
            ("ANTHROPIC_API_KEY".to_string(), relay.token.clone()),
            // MCP 설정의 `${RELAY_TOKEN}`이 채워지는 곳: 토큰은 파일이 아니라 환경으로만 간다
            (TOKEN_ENV.to_string(), relay.token.clone()),
            (
                "CLAUDE_CODE_MAX_OUTPUT_TOKENS".to_string(),
                model.max_output_tokens.to_string(),
            ),
            (
                "CLAUDE_CODE_MAX_RETRIES".to_string(),
                model.retries.to_string(),
            ),
        ]);
        env
    }
}

impl ModelRunner for ClaudeCodeRunner {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }

    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            let (model, call, relay) = cli::prepare(&self.config, &request)?;
            call.write_work(MCP_CONFIG, &relay.claude_config())
                .map_err(|_| cli::prepare_failed())?;
            let (mode, tail) = match &request.session {
                Some(session) => prepare_session(session, &request, &call, &relay).await?,
                None => (Mode::Ephemeral, None),
            };
            let spec = ModelSpec {
                attempt: request.attempt.clone(),
                program: self
                    .config
                    .program
                    .clone()
                    .unwrap_or_else(|| "claude".into()),
                args: Self::args(&model, &request.builtin, &relay, &mode)?,
                env: self.env(&model, &relay),
                cwd: BOT_MOUNT.into(),
                readonly_mounts: vec![Mount {
                    host: call.input.clone(),
                    guest: BOT_MOUNT.into(),
                }],
                scratch: Some(Mount {
                    host: call.work.clone(),
                    guest: WORK_MOUNT.into(),
                }),
                // 이어 받는 세션에는 원래 요청이 이미 들어 있다: 다시 보내면 같은 일을 또 시킬 수 있다
                stdin: match mode {
                    Mode::Resume(_) => None,
                    _ => Some(prompt::render_stdin(&request).into_bytes()),
                },
                timeout: request.limits.total,
                idle_timeout: request.limits.idle,
            };
            // 이어 받은 세션의 보고는 그 CLI 실행 몫뿐이다(앞 실행은 도구 승인·위임으로 끝나 결과 보고가 없다): 전체 합을 알 수 없다
            let parser = Box::new(Tracker::new(matches!(mode, Mode::Resume(_))));
            let output = match tail {
                None => {
                    driver::drive(
                        self.sandbox.as_ref(),
                        spec,
                        parser,
                        request.limits,
                        request.cancel.clone(),
                    )
                    .await
                }
                Some(tail) => {
                    self.drive_watched(spec, parser, request.limits, request.cancel.clone(), tail)
                        .await
                }
            };
            drop(call);
            output.map(cli::final_step)
        })
    }
}

impl ClaudeCodeRunner {
    /// CLI를 돌리는 동안 세션 파일을 따라 읽어 응답마다 묶음을 쓴다. 세션을 더 믿을 수 없으면(소유권 상실·확인 불일치·시간 초과) CLI를 끝내고
    /// 그 사유를 돌려준다.
    async fn drive_watched(
        &self,
        spec: ModelSpec,
        parser: Box<Tracker>,
        limits: super::Limits,
        cancel: watch::Receiver<bool>,
        mut tail: Tail,
    ) -> Result<Output, ModelError> {
        let (abort, aborted) = watch::channel(false);
        // 마지막 확인도 CLI와 같은 마감 시각과 취소 신호를 따른다(원장이 응답하지 않아도 전체 시간을 넘기지 않는다)
        let deadline = tokio::time::Instant::now() + limits.total;
        let mut finish_cancel = cancel.clone();
        let driven =
            driver::drive_with(self.sandbox.as_ref(), spec, parser, limits, cancel, aborted);
        let output = {
            let watching = async {
                loop {
                    tokio::time::sleep(TAIL_EVERY).await;
                    if let Err(error) = tail.poll().await {
                        return error;
                    }
                }
            };
            tokio::pin!(driven, watching);
            tokio::select! {
                output = &mut driven => output,
                error = &mut watching => {
                    let _ = abort.send(true);
                    let _ = driven.await; // 프로세스가 끝날 때까지 기다린다
                    return Err(error);
                }
            }
        }?;
        // 최종 답을 올리기 전의 마지막 확인: 저장하지 못했거나 원장에서 끝나지 않은 도구 호출이 있으면 그 답을 성공으로 올리지 않는다
        tokio::select! {
            biased;
            () = driver::cancelled(&mut finish_cancel) => Err(ModelError::Cancelled),
            checked = tokio::time::timeout_at(deadline, tail.finish()) => match checked {
                Ok(Ok(())) => Ok(output),
                Ok(Err(error)) => Err(error),
                Err(_) => Err(ModelError::Timeout),
            },
        }
    }
}

fn session_error(error: SessionError) -> ModelError {
    match error {
        SessionError::Lost => ModelError::Lost,
        SessionError::Unavailable => ModelError::Failed {
            code: "session_unavailable",
            transient: true,
        },
        SessionError::Permanent(code) => cli::failed(code),
    }
}

/// 세션 파일 시각(밀리초 UTC).
fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// 이 claim의 세션을 정한다. 되살릴 세션이 없으면 새로 시작하고, 있으면 아직 결과 없는 `tool_use`를 현재 권한·승인으로 마무리해(승인·위임을
/// 다시 기다려야 하면 `Parked`) 결과 줄을 체크포인트에 먼저 남긴 뒤 새 HOME의 세션 파일을 만든다.
async fn prepare_session(
    session: &Session,
    request: &ModelRequest,
    call: &CallDir,
    relay: &RelayChannel,
) -> Result<(Mode, Option<Tail>), ModelError> {
    let port = &session.0;
    let id = session_id(&request.attempt.profile, &request.attempt.run);
    let prefix = format!("mcp__{}__", relay.name);
    let relative = format!("claude/projects/{}/{id}.jsonl", project_dir(BOT_MOUNT));
    let saved = restore(port.steps()).map_err(|_| cli::failed("session_corrupt"))?;
    let Some(mut saved) = saved else {
        let journal = Journal::new(0, Default::default(), prefix);
        let tail = Tail::new(call.work.clone(), relative, 0, journal, session.clone());
        return Ok((Mode::Fresh(id), Some(tail)));
    };
    // 세션 파일은 VM이 쓴 것이다: 이미 결과가 있는 릴레이 도구도 결과 본문을 믿지 않고, 원래 계획(이름·입력)을 현재 권한·도구·인자로
    // 새 실행 없이 다시 읽어(끝난 호출의 값, 거절은 고정 단어) 그 값으로 덮어쓴다. 끝났다는 기록이 없으면 믿을 결과가 없으니 중단으로 표시한다
    let mut trusted = BTreeMap::new();
    for slot in saved.relay_results(&prefix) {
        let output = match port.recall(&slot).await {
            Ok(Some(output)) => output,
            Ok(None) => ToolOutput::notice("aborted"),
            Err(error) => return Err(session_error(error)),
        };
        trusted.insert(slot.tool_use.id.clone(), output);
    }
    saved
        .trust(&trusted)
        .map_err(|_| cli::failed("session_corrupt"))?;
    let mut outcomes = Vec::new();
    for slot in saved.pending() {
        match port.settle(&slot).await {
            Ok(Slot::Result(output)) => outcomes.push((slot.tool_use.id.clone(), output)),
            Ok(Slot::Parked) => return Err(ModelError::Parked),
            Err(error) => return Err(session_error(error)),
        }
    }
    let mut lines = saved.lines.clone();
    let mut next = saved.next_seq();
    if let Some(line) = result_line(&saved, &id, &outcomes, &timestamp()) {
        // 파일보다 체크포인트가 먼저다: 이 뒤에 멈춰도 다음 claim이 같은 결과 줄을 그대로 이어 받는다
        port.commit(vec![rec_step(next, &line)])
            .await
            .map_err(session_error)?;
        lines.push(line);
        next += 1;
    }
    let text = lines.join("\n") + "\n";
    call.write_work(&relative, &text)
        .map_err(|_| cli::prepare_failed())?;
    let journal = Journal::new(next, saved.known.clone(), prefix);
    let tail = Tail::new(
        call.work.clone(),
        relative,
        text.len() as u64,
        journal,
        session.clone(),
    );
    Ok((Mode::Resume(id), Some(tail)))
}

/// 마지막 `result` 이벤트
#[derive(Debug, Default)]
struct Finished {
    is_error: bool,
    text: String,
    /// API 오류의 HTTP 상태 (api_error_status)
    status: Option<u64>,
    input: u64,
    output: u64,
    /// 공급자가 실제로 보고한 이 CLI 실행의 전체 토큰 수(없으면 `None`)
    measured: Option<TokenUsage>,
}

/// JSONL 이벤트를 한 줄씩 먹고 마지막 결과를 기억한다.
#[derive(Default)]
struct Tracker {
    result: Option<Finished>,
    /// 이어 받은 세션이면 실측 합을 알 수 없다
    resumed: bool,
    /// 요청마다 공급자가 실제로 보고한 계량(`usage.rs`): 한 요청이라도 보고가 빠지면 전체 합을 알 수 없다
    calls: usage::Calls,
}

impl Tracker {
    fn new(resumed: bool) -> Self {
        Self {
            result: None,
            resumed,
            calls: usage::Calls::default(),
        }
    }
}

impl Parser for Tracker {
    fn feed(&mut self, line: &str) -> Option<Event> {
        let v: Value = serde_json::from_str(line).ok()?;
        match v["type"].as_str()? {
            "stream_event" => {
                let e = &v["event"];
                self.calls.feed(e);
                let kind = match e["type"].as_str()? {
                    "content_block_start" => e["content_block"]["type"].as_str()?,
                    "content_block_delta" => e["delta"]["type"].as_str()?,
                    _ => return None,
                };
                match kind {
                    "thinking" | "thinking_delta" => Some(Event::Thinking),
                    "text" | "text_delta" => Some(Event::Writing),
                    _ => None,
                }
            }
            "system" if v["subtype"] == "api_retry" => Some(Event::Retrying),
            "result" => {
                self.result = Some(Finished {
                    is_error: v["is_error"].as_bool().unwrap_or(false),
                    text: v["result"].as_str().unwrap_or_default().to_string(),
                    status: v["api_error_status"].as_u64(),
                    input: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
                    output: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
                    measured: self.calls.measured(&v["usage"]),
                });
                None
            }
            _ => None,
        }
    }

    fn has_result(&self) -> bool {
        self.result.is_some()
    }

    fn finish(self: Box<Self>) -> Result<Output, ModelError> {
        let Some(r) = self.result else {
            return Err(ModelError::Failed {
                code: "no_result",
                transient: false,
            });
        };
        if r.is_error {
            let (code, transient) = classify(&r.text, r.status);
            return Err(ModelError::Failed { code, transient });
        }
        if r.text.trim().is_empty() {
            return Err(ModelError::Failed {
                code: "empty_answer",
                transient: false,
            });
        }
        Ok(Output {
            text: r.text.trim().to_string(),
            usage: Usage {
                input_tokens: r.input,
                output_tokens: r.output,
            },
            // 공급자의 출력 한도로 잘린 답은 이 CLI가 부분 글을 결과로 내지 않고 오류로 끝나 여기까지 오지 않는다(위 `is_error`: 실패로 둔다)
            outcome: RunOutcome {
                truncated: false,
                usage: if self.resumed { None } else { r.measured },
            },
        })
    }
}

mod usage;

#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod usage_tests;
