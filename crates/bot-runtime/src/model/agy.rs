//! Antigravity CLI (`agy`) model runner with relay-only egress, RAM-only MCP token expansion, and ledger resume.

mod resume;
#[cfg(test)]
mod tests;

use super::{
    CliConfig, CliKind, CliModel, ModelError, ModelMeta, ModelRequest, ModelRunner, RelayChannel,
    RunnerKind, StepFuture, TOKEN_ENV, ToolKind, ToolSpec, Usage,
    cli::{self, BOT_MOUNT, WORK_MOUNT},
    driver::{self, Event, Output, Parser, classify},
    prompt,
};
use bot_core::{
    builtin::DELEGATE_TOOL,
    command::{RunOutcome, TokenUsage},
    profile::Builtin,
    sandbox::{ModelSandbox, ModelSpec, Mount},
    state::Risk,
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, sync::Arc};

/// AGY settings filename under `/work`, outside the read-only `/bot` directory.
pub const SETTINGS_CONFIG: &str = "agy.settings.json";
/// AGY MCP config template under `/work` containing `${RELAY_TOKEN}` (expanded to `/tmp/mcp_config.json` by `bot-init`).
pub const MCP_TEMPLATE: &str = "mcp.agy.template.json";
/// Safe cap below AGY's 24,000-byte rule truncation limit so `/bot/AGENTS.md` is never silently clipped.
pub(crate) const MAX_AGENTS_BYTES: usize = 22_000;
const MCP_LINK_TARGET: &str = "/tmp/mcp_config.json";
const MCP_LINK_RELATIVE: &str = "home/.gemini/config/mcp_config.json";

pub struct AgyRunner {
    sandbox: Arc<ModelSandbox>,
    config: CliConfig,
}

impl AgyRunner {
    pub fn new(sandbox: Arc<ModelSandbox>, config: CliConfig) -> Self {
        Self { sandbox, config }
    }

    /// Maps host model alias/effort to an AGY CLI `--model` argument; the broker always pins the upstream model URL.
    pub fn catalog_model(model: &CliModel) -> String {
        if model.id.starts_with("Gemini ") {
            return model.id.clone();
        }
        if model.id.starts_with("gemini-3.1-pro") {
            return if model.effort.eq_ignore_ascii_case("low") {
                "Gemini 3.1 Pro (Low)".to_owned()
            } else {
                "Gemini 3.1 Pro (High)".to_owned()
            };
        }
        model.id.clone()
    }

    fn passes_effort(model: &CliModel) -> bool {
        if model.effort.is_empty()
            || model.id.starts_with("Gemini ")
            || model.id.starts_with("gemini-3.1-pro")
        {
            return false;
        }
        !["-low", "-medium", "-high", "-xhigh", "-max"]
            .iter()
            .any(|suffix| model.id.ends_with(suffix))
    }

    pub fn settings(relay: &RelayChannel) -> String {
        json!({
            "modelProvider": "gemini",
            "toolPermission": "always-proceed",
            "permissions": {
                "deny": [
                    "command(*)",
                    "write_file(*)",
                    "read_url(*)",
                    "execute_url(*)",
                    "read_file(*)",
                ],
                "allow": [format!("mcp({}/*)", relay.name)],
            }
        })
        .to_string()
    }

    pub fn args(model: &CliModel, builtin: &BTreeSet<Builtin>) -> Result<Vec<String>, ModelError> {
        if !builtin.is_empty() {
            return Err(cli::failed("unsupported_builtin"));
        }
        let mut args: Vec<String> = [
            "--print=",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--disable-slash-commands",
            "--model",
            &Self::catalog_model(model),
        ]
        .map(String::from)
        .into();
        if Self::passes_effort(model) {
            args.extend(["--effort".into(), model.effort.clone()]);
        }
        args.extend([
            "--settings".into(),
            format!("{WORK_MOUNT}/{SETTINGS_CONFIG}"),
        ]);
        Ok(args)
    }

    fn env(&self, relay: &RelayChannel) -> Vec<(String, String)> {
        vec![
            ("HOME".into(), format!("{WORK_MOUNT}/home")),
            ("GOOGLE_GEMINI_BASE_URL".into(), relay.base_url.clone()),
            ("GEMINI_API_KEY".into(), relay.token.clone()),
            (TOKEN_ENV.into(), relay.token.clone()),
            ("DO_NOT_TRACK".into(), "1".into()),
        ]
    }
}

fn risk_label(risk: Risk) -> &'static str {
    match risk {
        Risk::Read => "read",
        Risk::Write => "write",
        Risk::Irreversible => "irreversible",
    }
}

fn render_tools_block(relay_name: &str, tools: &[ToolSpec]) -> String {
    if tools.is_empty() {
        return String::new();
    }
    let mut out = format!(
        "# Available relay MCP tools (`ServerName: \"{relay_name}\"`)\n\n\
         Invoke tools via `call_mcp_tool` with `ServerName: \"{relay_name}\"`, \
         `ToolName` set to the exact MCP name below, and `Arguments` matching its JSON schema.\n\n"
    );
    for tool in tools {
        let mcp_name = match tool.kind {
            ToolKind::Delegate => DELEGATE_TOOL.to_owned(),
            ToolKind::Skill => tool.id.mcp_name(),
        };
        let desc = tool
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        out.push_str(&format!(
            "- `{mcp_name}` (class: {}): {desc} Schema: {}\n",
            risk_label(tool.class),
            tool.input.0
        ));
    }
    out
}

/// Splits system instructions and lazy MCP schemas across `/bot/AGENTS.md` (<= 22 KB) and stdin so AGY never truncates rules or schemas.
pub(crate) fn layout_instructions(
    system: &str,
    relay_name: &str,
    tools: &[ToolSpec],
) -> (String, Option<String>) {
    let tools_block = render_tools_block(relay_name, tools);
    if tools_block.is_empty() && system.len() <= MAX_AGENTS_BYTES {
        return (system.to_owned(), None);
    }
    if !tools_block.is_empty() && system.len() + 2 + tools_block.len() <= MAX_AGENTS_BYTES {
        return (format!("{system}\n\n{tools_block}"), None);
    }
    if system.len() <= MAX_AGENTS_BYTES {
        return (system.to_owned(), Some(tools_block));
    }
    let split = split_boundary(system, MAX_AGENTS_BYTES);
    let head = system[..split].to_owned();
    let tail = system[split..].trim_start();
    let mut spill = String::new();
    if !tail.is_empty() {
        spill.push_str("# Additional system instructions\n");
        spill.push_str(tail);
    }
    if !tools_block.is_empty() {
        if !spill.is_empty() {
            spill.push_str("\n\n");
        }
        spill.push_str(&tools_block);
    }
    (head, (!spill.is_empty()).then_some(spill))
}

fn split_boundary(text: &str, max_bytes: usize) -> usize {
    let mut at = max_bytes.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    let mut window_start = at.saturating_sub(1024);
    while window_start < at && !text.is_char_boundary(window_start) {
        window_start += 1;
    }
    if let Some(rel) = text[window_start..at].rfind('\n') {
        let candidate = window_start + rel;
        if candidate > 0 {
            return candidate;
        }
    }
    at
}

fn render_stream_stdin(request: &ModelRequest, spill: Option<&str>) -> Vec<u8> {
    let rendered = prompt::render_stdin(request);
    let content = match spill {
        Some(prefix) => format!("{prefix}\n\n{rendered}"),
        None => rendered,
    };
    let mut line = json!({
        "event": "user",
        "message": { "content": content },
    })
    .to_string();
    line.push('\n');
    line.into_bytes()
}

impl ModelRunner for AgyRunner {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Loop
    }

    fn step(&self, mut request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            if !request.builtin.is_empty() {
                return Err(cli::failed("unsupported_builtin"));
            }
            let (model, call, relay) = cli::prepare(&self.config, &request)?;
            let prefix = format!("mcp__{}__", relay.name);
            let resumed = match &request.session {
                Some(session) => match resume::prepare(session, &prefix).await? {
                    Some(exchanges) => {
                        request.exchanges.extend(exchanges);
                        true
                    }
                    None => false,
                },
                None => false,
            };
            let (agents_md, spill) =
                layout_instructions(&request.system, &relay.name, &request.tools);
            call.write_input("AGENTS.md", &agents_md)
                .map_err(|_| cli::prepare_failed())?;
            call.write_work(SETTINGS_CONFIG, &Self::settings(&relay))
                .map_err(|_| cli::prepare_failed())?;
            call.write_work(MCP_TEMPLATE, &format!("{}\n", relay.agy_mcp_template()))
                .map_err(|_| cli::prepare_failed())?;
            #[cfg(unix)]
            call.symlink_work(MCP_LINK_RELATIVE, MCP_LINK_TARGET)
                .map_err(|_| cli::prepare_failed())?;
            #[cfg(not(unix))]
            return Err(cli::prepare_failed());
            let spec = ModelSpec {
                attempt: request.attempt.clone(),
                program: self
                    .config
                    .program
                    .clone()
                    .unwrap_or_else(|| CliKind::Agy.program().into()),
                args: Self::args(&model, &request.builtin)?,
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
                stdin: Some(render_stream_stdin(&request, spill.as_deref())),
                timeout: request.limits.total,
                idle_timeout: request.limits.idle,
            };
            let parser = Box::new(AgyTracker::new(resumed));
            let output = driver::drive(
                self.sandbox.as_ref(),
                spec,
                parser,
                request.limits,
                request.cancel.clone(),
            )
            .await;
            drop(call);
            output.map(cli::final_step)
        })
    }

    fn meta(&self, alias: &str) -> Option<ModelMeta> {
        self.config.models.get(alias).map(|m| ModelMeta {
            provider: CliKind::Agy.provider_name(),
            model: m.id.clone(),
        })
    }
}

#[derive(Debug)]
struct Finished {
    is_error: bool,
    response: String,
    error: String,
    input: u64,
    output: u64,
    has_usage: bool,
}

#[derive(Default)]
struct AgyTracker {
    resumed: bool,
    step_usages: Vec<(Option<u64>, Option<u64>)>,
    last_error: Option<String>,
    result: Option<Finished>,
}

impl AgyTracker {
    fn new(resumed: bool) -> Self {
        Self {
            resumed,
            ..Self::default()
        }
    }

    fn measured(&self, finished: &Finished) -> Option<TokenUsage> {
        if self.resumed || !finished.has_usage || finished.input == 0 || finished.output == 0 {
            return None;
        }
        if !self.step_usages.is_empty() {
            let (mut prompt_sum, mut completion_sum) = (0u64, 0u64);
            for (input, output) in &self.step_usages {
                let (p, c) = ((*input)?, (*output)?);
                if p == 0 || c == 0 {
                    return None;
                }
                prompt_sum = prompt_sum.saturating_add(p);
                completion_sum = completion_sum.saturating_add(c);
            }
            if prompt_sum != finished.input || completion_sum != finished.output {
                return None;
            }
        }
        Some(TokenUsage {
            prompt: finished.input,
            completion: finished.output,
        })
    }
}

impl Parser for AgyTracker {
    fn feed(&mut self, line: &str) -> Option<Event> {
        let v: Value = serde_json::from_str(line).ok()?;
        match v["event"].as_str()? {
            "step_update" => {
                let su = &v["step_update"];
                match su["step_type"].as_str()? {
                    "agent_response" => {
                        if su["state"].as_str() == Some("DONE")
                            && let Some(usage) = su.get("usage").and_then(Value::as_object)
                        {
                            self.step_usages.push((
                                usage.get("input_tokens").and_then(Value::as_u64),
                                usage.get("output_tokens").and_then(Value::as_u64),
                            ));
                        }
                        if su["text_delta"]
                            .as_str()
                            .is_some_and(|delta| !delta.is_empty())
                        {
                            Some(Event::Writing)
                        } else {
                            Some(Event::Thinking)
                        }
                    }
                    "tool" => Some(Event::Thinking),
                    "error_message" => {
                        if let Some(msg) = su["error"]
                            .as_str()
                            .or_else(|| su["message"].as_str())
                            .filter(|s| !s.is_empty())
                        {
                            self.last_error = Some(msg.to_owned());
                        }
                        None
                    }
                    _ => None,
                }
            }
            "result" => {
                let r = &v["result"];
                let status = r["status"].as_str().unwrap_or_default();
                let usage = r.get("usage").and_then(Value::as_object);
                self.result = Some(Finished {
                    is_error: status != "SUCCESS",
                    response: r["response"].as_str().unwrap_or_default().to_owned(),
                    error: r["error"].as_str().unwrap_or_default().to_owned(),
                    input: usage
                        .and_then(|u| u.get("input_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    output: usage
                        .and_then(|u| u.get("output_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    has_usage: usage.is_some(),
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
        let Some(ref finished) = self.result else {
            return Err(ModelError::Failed {
                code: "no_result",
                transient: false,
            });
        };
        if finished.is_error {
            let raw = if !finished.error.is_empty() {
                finished.error.as_str()
            } else if let Some(ref msg) = self.last_error {
                msg.as_str()
            } else {
                finished.response.as_str()
            };
            let (code, transient) = classify(raw, None);
            return Err(ModelError::Failed { code, transient });
        }
        if finished.response.trim().is_empty() {
            return Err(ModelError::Failed {
                code: "empty_answer",
                transient: false,
            });
        }
        let measured = self.measured(finished);
        Ok(Output {
            text: finished.response.trim().to_owned(),
            usage: Usage {
                input_tokens: finished.input,
                output_tokens: finished.output,
            },
            outcome: RunOutcome {
                truncated: false,
                usage: measured,
            },
        })
    }
}
