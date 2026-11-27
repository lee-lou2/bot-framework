//! Claude Code CLI (`claude -p`) model runner with relay-only egress and session checkpointing.

use super::{
    CliConfig, CliModel, ModelError, ModelMeta, ModelRequest, ModelRunner, RelayChannel,
    RunnerKind, StepFuture, TOKEN_ENV, ToolOutput, Usage,
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

const TAIL_EVERY: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    Ephemeral,
    Fresh(String),
    Resume(String),
}

/// Disables nonessential traffic, telemetry, auto-updates, bundled skills, and auto-memory; mirrors `images/model/Containerfile`.
const FIXED_ENV: [(&str, &str); 12] = [
    ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ("DISABLE_TELEMETRY", "1"),
    ("DISABLE_AUTOUPDATER", "1"),
    ("DISABLE_ERROR_REPORTING", "1"),
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    ("CLAUDE_CODE_DISABLE_BUNDLED_SKILLS", "1"),
    ("DISABLE_UPDATES", "1"),
    ("DO_NOT_TRACK", "1"),
    ("DISABLE_FEEDBACK_COMMAND", "1"),
    ("CLAUDE_CODE_DISABLE_FEATURE_FLAGS", "1"),
    ("CLAUDE_CODE_DISABLE_TERMINAL_TITLE", "1"),
    ("CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL", "1"),
];

/// MCP config filename under `/work`, outside the read-only `/bot` directory readable by the model.
pub const MCP_CONFIG: &str = "mcp.claude.json";

pub struct ClaudeCodeRunner {
    sandbox: Arc<ModelSandbox>,
    config: CliConfig,
}

impl ClaudeCodeRunner {
    pub fn new(sandbox: Arc<ModelSandbox>, config: CliConfig) -> Self {
        Self { sandbox, config }
    }

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
            // `--bare` supports only `Read`; fail closed rather than substituting another tool.
            return Err(cli::failed("unsupported_builtin"));
        };
        let instruction = match mode {
            Mode::Resume(_) => prompt::RESUME_INSTRUCTION,
            _ => prompt::INSTRUCTION,
        };
        // `--bare` does not auto-load `AGENTS.md`, so attach it explicitly via `--append-system-prompt-file`.
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
            // Populates `${RELAY_TOKEN}` in `mcp.claude.json` so the session token never touches disk.
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
                // Resumed sessions already contain the original prompt; resending it can duplicate work.
                stdin: match mode {
                    Mode::Resume(_) => None,
                    _ => Some(prompt::render_stdin(&request).into_bytes()),
                },
                timeout: request.limits.total,
                idle_timeout: request.limits.idle,
            };
            // Resumed sessions only report usage for the current invocation, not prior parked attempts.
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

    fn meta(&self, alias: &str) -> Option<ModelMeta> {
        self.config.models.get(alias).map(|m| ModelMeta {
            provider: "anthropic",
            model: m.id.clone(),
        })
    }
}

impl ClaudeCodeRunner {
    async fn drive_watched(
        &self,
        spec: ModelSpec,
        parser: Box<Tracker>,
        limits: super::Limits,
        cancel: watch::Receiver<bool>,
        mut tail: Tail,
    ) -> Result<Output, ModelError> {
        let (abort, aborted) = watch::channel(false);
        // Final session verification shares the same total deadline and cancel signal as the CLI run.
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
                    let _ = driven.await;
                    return Err(error);
                }
            }
        }?;
        // Refuse to emit a final answer if any relay call was unsaved or unfinished in the ledger.
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

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

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
    // Session files are VM-authored: overwrite every answered relay tool result from trusted host records without re-executing.
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
        // Checkpoint before writing the file so a crash after this point restores the exact same result line.
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

#[derive(Debug, Default)]
struct Finished {
    is_error: bool,
    text: String,
    status: Option<u64>,
    input: u64,
    output: u64,
    measured: Option<TokenUsage>,
}

#[derive(Default)]
struct Tracker {
    result: Option<Finished>,
    resumed: bool,
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
            // CLI reports output-limit truncation via `is_error`, so reaching here means `truncated: false`.
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
