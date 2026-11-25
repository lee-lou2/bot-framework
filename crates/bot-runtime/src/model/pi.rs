//! Pi Coding Agent CLI(`pi --mode json`) 실행기.
//!
//! 루프 내장(`RunnerKind::Loop`) 실행기다: CLI가 자기 도구 순환을 돌리고 워커는 차례를 제어하지 않는다. 항상 `ModelStep::Final`을 돌려준다.
//! 샌드박스(VM) 안에서 `/bot`(읽기 전용 입력 폴더)을 작업 폴더로 실행한다. 지침은 `AGENTS.md`를 Pi가 표준대로 읽는다(프로젝트 신뢰 `--approve`).
//! 모델과 도구는 `ModelRequest::relay`(run별 브로커 프런트: 주소 + 이 시도의 세션 토큰)로만 나간다. 번들 확장 중 도구 서버(`builtin:mcp`)와
//! 지연 도구 검색(`builtin:tool-search`)만 켜고(`-ne -e …`) 내장 도구는 에이전트가 `builtin`에 명시한 읽기 전용 도구만 연다
//! (`read`·`grep`·`find` ← Read·Grep·Glob; 기본은 없음, `ls`·셸·웹·파일 쓰기는 열지 않는다). 공식 문서: `@earendil-works/pi-coding-agent` `docs/` (`cli.md` · `models-configuration.md` · `mcp.md` · `json.md`)
//!
//! 중계 주소에 붙이는 설정은 호출마다 호출 전용 쓰기 공간 안에 만드는 `PI_CODING_AGENT_DIR`(`pi-agent/`)에 둔다:
//! - `models.json`: 모델 이름에 맞는 프로토콜(`claude-*` → `anthropic-messages`, 그 밖 → `openai-responses` + `/v1`)과 주소 · 최대 출력 토큰 · 키 환경변수(`$PI_API_KEY`).
//!   Pi는 `thinkingLevelMap`에 적지 않은 `xhigh` · `max`를 쓰지 못하고 모델이 받는 단계로 낮춰 보내므로 두 갈래 모두 `low`~`max`를 그대로 대응시킨다.
//! - `settings.json`: 원격 측정을 끄고 `retry.maxRetries`에 모델의 `retries`를 넣는다.
//! - `mcp.json`: 중계 끝점으로 만든 설정(`RelayChannel::pi_config`, `exposure: "deferred"`). 헤더는 `Bearer ${RELAY_TOKEN}` 자리표시뿐이고 값은 환경변수로만 간다.
//!
//! 결과는 JSONL 이벤트다: `message_update`(생각 · 글자 조각) · `auto_retry_start`(재시도) · `message_end`(턴별 확정 메시지와 토큰 · 오류) · `agent_settled`(종료).
//! 환경의 비밀은 이 시도에 발급한 세션 토큰(`PI_API_KEY`·`RELAY_TOKEN`)뿐이다(실제 제공자 키는 VM에 없다). 어떤 파일에도 토큰을 쓰지 않는다.

use super::{
    CliConfig, CliModel, ModelError, ModelRequest, ModelRunner, RelayChannel, RunnerKind,
    StepFuture, TOKEN_ENV, Usage,
    cli::{self, BOT_MOUNT, WORK_MOUNT},
    driver::{self, Event, Output, Parser, classify},
    prompt,
};
use bot_core::{
    profile::Builtin,
    sandbox::{ModelSandbox, ModelSpec, Mount},
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

/// 호출 전용 쓰기 공간 안의 HOME과 Pi 설정 폴더
const HOME_DIR: &str = "pi-home";
const AGENT_DIR: &str = "pi-agent";
/// Pi가 모델 키를 읽을 환경변수 이름 (`models.json`의 `$PI_API_KEY`가 가리킨다)
const KEY_ENV: &str = "PI_API_KEY";

pub struct PiRunner {
    sandbox: Arc<ModelSandbox>,
    config: CliConfig,
}

impl PiRunner {
    pub fn new(sandbox: Arc<ModelSandbox>, config: CliConfig) -> Self {
        Self { sandbox, config }
    }

    /// 명령 인자. 도구는 명시한 읽기 전용 내장 도구 + 지연 도구 검색(`tool_search`)뿐이다.
    pub fn args(model: &CliModel, provider: &str, builtin: &BTreeSet<Builtin>) -> Vec<String> {
        let mut tools: Vec<&str> = builtin
            .iter()
            .map(|b| match b {
                Builtin::Read => "read",
                Builtin::Grep => "grep",
                Builtin::Glob => "find",
            })
            .collect();
        tools.push("tool_search");
        let mut a: Vec<String> = [
            "--mode",
            "json",
            "--no-session",
            "--approve",
            "-ne",
            "-e",
            "builtin:mcp",
            "-e",
            "builtin:tool-search",
        ]
        .map(String::from)
        .into();
        a.extend(["--tools".to_string(), tools.join(",")]);
        a.extend(
            [
                "--provider",
                provider,
                "--model",
                &model.id,
                "--thinking",
                &model.effort,
                "--",
                prompt::INSTRUCTION,
            ]
            .map(String::from),
        );
        a
    }

    fn env(&self, relay: &RelayChannel) -> Vec<(String, String)> {
        [
            ("HOME".to_string(), format!("{WORK_MOUNT}/{HOME_DIR}")),
            (
                "PI_CODING_AGENT_DIR".to_string(),
                format!("{WORK_MOUNT}/{AGENT_DIR}"),
            ),
            ("PI_OFFLINE".to_string(), "1".to_string()),
            ("PI_SKIP_VERSION_CHECK".to_string(), "1".to_string()),
            ("PI_TELEMETRY".to_string(), "0".to_string()),
            (KEY_ENV.to_string(), relay.token.clone()),
            // `mcp.json`의 `${RELAY_TOKEN}`이 채워지는 곳: 토큰은 파일이 아니라 환경으로만 간다
            (TOKEN_ENV.to_string(), relay.token.clone()),
        ]
        .into()
    }
}

/// 모델 이름에 맞춰 Pi 프로바이더의 프로토콜 · 주소 · 옵션을 만든다.
/// - `claude-*`: Anthropic Messages(`anthropic-messages`, 주소 그대로, 적응형 사고 켜짐).
/// - 그 밖(`gpt-*` 등): OpenAI Responses(`openai-responses`, `…/v1`) — 중계가 이 모델의 원래 프로토콜로 받는다.
fn models_config(model: &CliModel, provider: &str, host: &str) -> String {
    let levels = json!({
        "off": null, "minimal": null, "low": "low", "medium": "medium", "high": "high", "xhigh": "xhigh", "max": "max"
    });
    let (api, base_url, extra) = if model.id.starts_with("claude-") {
        (
            "anthropic-messages",
            host.to_string(),
            json!({"thinkingLevelMap": levels, "compat": {"forceAdaptiveThinking": true, "supportsTemperature": false}}),
        )
    } else {
        (
            "openai-responses",
            format!("{host}/v1"),
            json!({"thinkingLevelMap": levels}),
        )
    };
    let mut entry = json!({
        "id": model.id,
        "reasoning": true,
        "input": ["text", "image"],
        "contextWindow": model.context_window,
        "maxTokens": model.max_output_tokens,
    });
    if let (Some(dst), Some(src)) = (entry.as_object_mut(), extra.as_object()) {
        for (k, v) in src {
            dst.insert(k.clone(), v.clone());
        }
    }
    json!({
        "providers": {
            provider: {
                "baseUrl": base_url,
                "api": api,
                "authHeader": true,
                "headers": {"User-Agent": "pi"},
                "apiKey": format!("${KEY_ENV}"),
                "models": [entry],
            }
        }
    })
    .to_string()
}

fn settings_config(model: &CliModel) -> String {
    json!({"enableInstallTelemetry": false, "retry": {"enabled": model.retries > 0, "maxRetries": model.retries}}).to_string()
}

impl ModelRunner for PiRunner {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }

    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            let (model, call, relay) = cli::prepare(&self.config, &request)?;
            let provider = &self.config.pi_provider;
            let write = |name: &str, content: &str| {
                call.write_work(&format!("{AGENT_DIR}/{name}"), content)
                    .map_err(|_| cli::prepare_failed())
            };
            write(
                "models.json",
                &models_config(&model, provider, &relay.base_url),
            )?;
            write("settings.json", &settings_config(&model))?;
            write("mcp.json", &relay.pi_config())?;
            let spec = ModelSpec {
                attempt: request.attempt.clone(),
                program: self.config.program.clone().unwrap_or_else(|| "pi".into()),
                args: Self::args(&model, provider, &request.builtin),
                env: self.env(&relay),
                cwd: BOT_MOUNT.into(),
                readonly_mounts: vec![Mount {
                    host: call.input.clone(),
                    guest: BOT_MOUNT.into(),
                }],
                scratch: Some(Mount {
                    host: call.work.clone(),
                    guest: WORK_MOUNT.into(),
                }),
                stdin: Some(prompt::render_stdin(&request).into_bytes()),
                timeout: request.limits.total,
                idle_timeout: request.limits.idle,
            };
            let output = driver::drive(
                self.sandbox.as_ref(),
                spec,
                Box::<Tracker>::default(),
                request.limits,
                request.cancel.clone(),
            )
            .await;
            drop(call);
            output.map(cli::final_step)
        })
    }
}

#[derive(Default)]
struct Tracker {
    settled: bool,
    aborted: bool,
    /// 마지막 어시스턴트 턴의 종료 사유와 오류 문구 (재시도 뒤 성공하면 덮어써진다)
    last_stop: String,
    last_error: String,
    /// 글이 있는 어시스턴트 메시지마다 그 메시지의 글. 마지막 것이 최종 답이다.
    answers: Vec<String>,
    input: u64,
    output: u64,
}

impl Parser for Tracker {
    fn feed(&mut self, line: &str) -> Option<Event> {
        let v: Value = serde_json::from_str(line).ok()?;
        match v["type"].as_str()? {
            "message_update" => match v["assistantMessageEvent"]["type"].as_str()? {
                "thinking_start" | "thinking_delta" => Some(Event::Thinking),
                "text_start" | "text_delta" => Some(Event::Writing),
                _ => None,
            },
            "auto_retry_start" => Some(Event::Retrying),
            "auto_retry_end" => {
                if v["success"] == false
                    && let Some(err) = v["finalError"].as_str()
                    && !err.is_empty()
                {
                    self.last_stop = "error".to_string();
                    self.last_error = err.to_string();
                }
                None
            }
            "message_end" if v["message"]["role"] == "assistant" => {
                let m = &v["message"];
                let u = &m["usage"];
                self.input +=
                    u["input"].as_u64().unwrap_or(0) + u["cacheRead"].as_u64().unwrap_or(0);
                self.output += u["output"].as_u64().unwrap_or(0);
                self.last_stop = m["stopReason"].as_str().unwrap_or_default().to_string();
                self.last_error = m["errorMessage"].as_str().unwrap_or_default().to_string();
                if let Some(blocks) = m["content"].as_array() {
                    let text: String = blocks
                        .iter()
                        .filter(|b| b["type"] == "text")
                        .filter_map(|b| b["text"].as_str())
                        .collect();
                    if !text.trim().is_empty() {
                        self.answers.push(text.trim().to_string());
                    }
                }
                None
            }
            "agent_settled" => {
                self.settled = true;
                self.aborted = v["aborted"].as_bool().unwrap_or(false);
                None
            }
            _ => None,
        }
    }

    fn has_result(&self) -> bool {
        self.settled
    }

    fn finish(mut self: Box<Self>) -> Result<Output, ModelError> {
        if !self.settled {
            return Err(ModelError::Failed {
                code: "no_result",
                transient: false,
            });
        }
        if self.aborted || self.last_stop == "error" || self.last_stop == "aborted" {
            let (code, transient) = classify(&self.last_error, None);
            return Err(ModelError::Failed { code, transient });
        }
        let Some(text) = self.answers.pop() else {
            return Err(ModelError::Failed {
                code: "empty_answer",
                transient: false,
            });
        };
        // 이 실행기의 보고는 실측 여부를 구분하지 못한다(없으면 0): API에 내보낼 실측으로 쓰지 않는다
        Ok(Output {
            text,
            usage: Usage {
                input_tokens: self.input,
                output_tokens: self.output,
            },
            outcome: Default::default(),
        })
    }
}

#[cfg(test)]
mod tests;
