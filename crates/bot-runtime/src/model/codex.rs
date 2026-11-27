//! Codex CLI (`codex exec`) model runner with relay-only egress and rollout JSONL session recovery.

mod open;
mod restore;
mod session;
mod tail;
#[cfg(test)]
mod tests;

use self::{
    restore::{output_line, restore, restored_rollout_path},
    session::Journal,
    tail::CodexTail,
};
use super::{
    CliConfig, CliKind, CliModel, ModelError, ModelMeta, ModelRequest, ModelRunner, RelayChannel,
    RunnerKind, StepFuture, TOKEN_ENV, ToolOutput, Usage,
    cli::{self, BOT_MOUNT, CallDir, WORK_MOUNT, session_error, timestamp},
    driver::{self, Event, Output, Parser, classify},
    prompt,
    session::{Session, Slot, rec_step},
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
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Mode {
    Ephemeral,
    Fresh,
    Resume(String),
}

/// Built-in Codex features disabled inside the model VM so only relay MCP tools remain reachable.
const DISABLED_FEATURES: [&str; 11] = [
    "shell_tool",
    "apps",
    "browser_use",
    "computer_use",
    "plugins",
    "goals",
    "view_image",
    "multi_agent",
    "sleep_tool",
    "in_app_browser",
    "in_app_chat",
];

pub struct CodexRunner {
    sandbox: Arc<ModelSandbox>,
    config: CliConfig,
}

fn effort_ok(effort: &str) -> bool {
    !effort.is_empty()
        && effort.len() <= 32
        && effort
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

impl CodexRunner {
    pub fn new(sandbox: Arc<ModelSandbox>, config: CliConfig) -> Self {
        Self { sandbox, config }
    }

    pub fn args(
        model: &CliModel,
        builtin: &BTreeSet<Builtin>,
        relay: &RelayChannel,
        mode: &Mode,
    ) -> Result<Vec<String>, ModelError> {
        if !builtin.is_empty() {
            return Err(cli::failed("unsupported_builtin"));
        }
        let mut args: Vec<String> = vec!["exec".into()];
        if matches!(mode, Mode::Resume(_)) {
            args.push("resume".into());
            // Disable cwd filtering inside the per-attempt scratch dir so restored rollouts always match.
            args.push("--all".into());
        }
        args.extend(
            [
                "--json",
                "--skip-git-repo-check",
                "--ignore-user-config",
                "--ignore-rules",
                "--dangerously-bypass-approvals-and-sandbox",
                "--model",
                &model.id,
            ]
            .map(String::from),
        );
        if matches!(mode, Mode::Ephemeral) {
            args.push("--ephemeral".into());
        }
        if effort_ok(&model.effort) {
            args.extend([
                "-c".into(),
                format!("model_reasoning_effort=\"{}\"", model.effort),
            ]);
        }
        let provider = &relay.name;
        let configs = [
            format!("model_provider=\"{provider}\""),
            format!("model_providers.{provider}.name=\"{provider}\""),
            format!(
                "model_providers.{provider}.base_url=\"{}\"",
                relay.codex_base_url()
            ),
            format!("model_providers.{provider}.env_key=\"{TOKEN_ENV}\""),
            format!("model_providers.{provider}.wire_api=\"responses\""),
            format!("mcp_servers.{provider}.url=\"{}\"", relay.mcp_url()),
            format!("mcp_servers.{provider}.bearer_token_env_var=\"{TOKEN_ENV}\""),
            format!("mcp_servers.{provider}.enabled=true"),
            "web_search=\"disabled\"".into(),
        ];
        for entry in configs {
            args.push("-c".into());
            args.push(entry);
        }
        for feature in DISABLED_FEATURES {
            args.push("--disable".into());
            args.push(feature.into());
        }
        match mode {
            Mode::Ephemeral | Mode::Fresh => args.push("-".into()),
            Mode::Resume(thread_id) => {
                args.push(thread_id.clone());
                args.push(prompt::RESUME_INSTRUCTION.into());
            }
        }
        Ok(args)
    }

    fn env(&self, relay: &RelayChannel) -> Vec<(String, String)> {
        vec![
            ("HOME".into(), format!("{WORK_MOUNT}/home")),
            ("CODEX_HOME".into(), format!("{WORK_MOUNT}/codex")),
            (TOKEN_ENV.into(), relay.token.clone()),
            ("NO_COLOR".into(), "1".into()),
            ("DO_NOT_TRACK".into(), "1".into()),
        ]
    }
}

impl ModelRunner for CodexRunner {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }

    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            if !request.builtin.is_empty() {
                return Err(cli::failed("unsupported_builtin"));
            }
            let (model, call, relay) = cli::prepare(&self.config, &request)?;
            let (mode, tail) = match &request.session {
                Some(session) => prepare_session(session, &call, &relay).await?,
                None => (Mode::Ephemeral, None),
            };
            let spec = ModelSpec {
                attempt: request.attempt.clone(),
                program: self
                    .config
                    .program
                    .clone()
                    .unwrap_or_else(|| CliKind::Codex.program().into()),
                args: Self::args(&model, &request.builtin, &relay, &mode)?,
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
                stdin: match mode {
                    Mode::Resume(_) => None,
                    Mode::Ephemeral | Mode::Fresh => {
                        Some(prompt::render_stdin(&request).into_bytes())
                    }
                },
                timeout: request.limits.total,
                idle_timeout: request.limits.idle,
            };
            let parser = Box::new(CodexTracker::new(matches!(mode, Mode::Resume(_))));
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
                    driver::drive_watched(
                        self.sandbox.as_ref(),
                        spec,
                        parser,
                        request.limits,
                        request.cancel.clone(),
                        tail,
                    )
                    .await
                }
            };
            drop(call);
            output.map(cli::final_step)
        })
    }

    fn meta(&self, alias: &str) -> Option<ModelMeta> {
        self.config.models.get(alias).map(|m| ModelMeta {
            provider: CliKind::Codex.provider_name(),
            model: m.id.clone(),
        })
    }
}

async fn prepare_session(
    session: &Session,
    call: &CallDir,
    relay: &RelayChannel,
) -> Result<(Mode, Option<CodexTail>), ModelError> {
    let port = &session.0;
    let prefix = format!("mcp__{}__", relay.name);
    let saved = restore(port.steps()).map_err(|_| cli::failed("session_corrupt"))?;
    let Some(mut saved) = saved else {
        let journal = Journal::new(0, Default::default(), prefix);
        let tail = CodexTail::new(call.work.clone(), None, 0, journal, session.clone());
        return Ok((Mode::Fresh, Some(tail)));
    };
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
    let mut next_ordinal = saved.next_ordinal();
    if !outcomes.is_empty() {
        let now = timestamp();
        let mut commit_steps = Vec::with_capacity(outcomes.len());
        let mut appended = Vec::with_capacity(outcomes.len());
        for (call_id, output) in &outcomes {
            let line = output_line(call_id, output, &now, next_ordinal);
            if let Some(ord) = next_ordinal.as_mut() {
                *ord = ord.saturating_add(1);
            }
            commit_steps.push(rec_step(next, &line));
            appended.push(line);
            next += 1;
        }
        // Checkpoint synthetic tool outputs before writing the rollout file so crash recovery is deterministic.
        port.commit(commit_steps).await.map_err(session_error)?;
        lines.extend(appended);
    }
    let relative = restored_rollout_path(&saved.thread_id);
    let text = lines.join("\n") + "\n";
    call.write_work(&relative, &text)
        .map_err(|_| cli::prepare_failed())?;
    let journal = Journal::new(next, saved.known.clone(), prefix);
    let tail = CodexTail::new(
        call.work.clone(),
        Some(relative),
        text.len() as u64,
        journal,
        session.clone(),
    );
    Ok((Mode::Resume(saved.thread_id), Some(tail)))
}

#[derive(Default)]
struct CodexTracker {
    resumed: bool,
    completed: bool,
    failed_message: Option<String>,
    text: String,
    input_tokens: u64,
    output_tokens: u64,
    has_usage: bool,
}

impl CodexTracker {
    fn new(resumed: bool) -> Self {
        Self {
            resumed,
            ..Self::default()
        }
    }
}

impl Parser for CodexTracker {
    fn feed(&mut self, line: &str) -> Option<Event> {
        let v: Value = serde_json::from_str(line).ok()?;
        match v["type"].as_str()? {
            "turn.started" => Some(Event::Thinking),
            "item.started" => match v["item"]["type"].as_str()? {
                "reasoning" | "mcp_tool_call" => Some(Event::Thinking),
                "agent_message" => Some(Event::Writing),
                _ => None,
            },
            "item.completed" => match v["item"]["type"].as_str()? {
                "agent_message" => {
                    if let Some(text) = v["item"]["text"].as_str() {
                        self.text = text.to_owned();
                    }
                    Some(Event::Writing)
                }
                "error" => {
                    if let Some(msg) = v["item"]["message"].as_str() {
                        self.failed_message = Some(msg.to_owned());
                    }
                    None
                }
                _ => None,
            },
            "error" => {
                let msg = v["message"].as_str().unwrap_or_default();
                if msg.to_ascii_lowercase().contains("reconnect")
                    || msg.to_ascii_lowercase().contains("retry")
                {
                    return Some(Event::Retrying);
                }
                if !msg.is_empty() {
                    self.failed_message = Some(msg.to_owned());
                }
                None
            }
            "turn.completed" => {
                self.completed = true;
                if let Some(usage) = v.get("usage").and_then(Value::as_object) {
                    let input = usage
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let output = usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.input_tokens = self.input_tokens.saturating_add(input);
                    self.output_tokens = self.output_tokens.saturating_add(output);
                    self.has_usage = true;
                }
                None
            }
            "turn.failed" => {
                let msg = v["error"]["message"]
                    .as_str()
                    .or_else(|| v["message"].as_str())
                    .unwrap_or("turn failed");
                self.failed_message = Some(msg.to_owned());
                None
            }
            _ => None,
        }
    }

    fn has_result(&self) -> bool {
        self.completed || self.failed_message.is_some()
    }

    fn finish(self: Box<Self>) -> Result<Output, ModelError> {
        if !self.completed {
            if let Some(msg) = self.failed_message {
                let (code, transient) = classify(&msg, None);
                return Err(ModelError::Failed { code, transient });
            }
            return Err(ModelError::Failed {
                code: "no_result",
                transient: false,
            });
        }
        if self.text.trim().is_empty() {
            return Err(ModelError::Failed {
                code: "empty_answer",
                transient: false,
            });
        }
        let measured =
            (!self.resumed && self.has_usage && self.input_tokens > 0 && self.output_tokens > 0)
                .then_some(TokenUsage {
                    prompt: self.input_tokens,
                    completion: self.output_tokens,
                });
        Ok(Output {
            text: self.text.trim().to_owned(),
            usage: Usage {
                input_tokens: self.input_tokens,
                output_tokens: self.output_tokens,
            },
            outcome: RunOutcome {
                truncated: false,
                usage: measured,
            },
        })
    }
}
