//! Shared CLI runner configuration, per-invocation isolated call directories, and output conversion.
//!
//! Each invocation exclusively creates a `0700` directory with random suffix (`in/` read-only mount,
//! `work/` scratch mount) so concurrent claims never collide and drop removes only its own directory.

use super::{ModelError, ModelRequest, ModelStep, Output, RelayChannel, SessionError};
use bot_core::id::Attempt;
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

pub(crate) const BOT_MOUNT: &str = "/bot";
pub(crate) const WORK_MOUNT: &str = "/work";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum CliKind {
    Claude,
    Codex,
    Agy,
}

impl CliKind {
    pub fn program(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Agy => "agy",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude-cli",
            Self::Codex => "codex-cli",
            Self::Agy => "agy-cli",
        }
    }

    pub fn provider_name(self) -> &'static str {
        match self {
            Self::Claude => "anthropic",
            Self::Codex => "openai",
            Self::Agy => "google",
        }
    }

    pub fn vendor(self) -> bot_core::auth::CliVendor {
        match self {
            Self::Claude => bot_core::auth::CliVendor::Claude,
            Self::Codex => bot_core::auth::CliVendor::Codex,
            Self::Agy => bot_core::auth::CliVendor::Google,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliModel {
    pub id: String,
    pub effort: String,
    pub max_output_tokens: u32,
    pub retries: u32,
}

#[derive(Clone, Debug)]
pub struct CliConfig {
    pub models: BTreeMap<String, CliModel>,
    pub workspace: PathBuf,
    pub program: Option<String>,
    /// Set only when the sandbox confines file reads to mounted directories (e.g. VM); required for `Agent.builtin`.
    pub confined_reads: bool,
}

impl CliConfig {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            models: BTreeMap::new(),
            workspace: workspace.into(),
            program: None,
            confined_reads: false,
        }
    }

    pub fn with_confined_reads(mut self) -> Self {
        self.confined_reads = true;
        self
    }

    pub fn with_model(mut self, alias: impl Into<String>, model: CliModel) -> Self {
        self.models.insert(alias.into(), model);
        self
    }
}

pub(crate) struct CallDir {
    root: PathBuf,
    pub input: PathBuf,
    pub work: PathBuf,
}

fn safe(part: &str) -> String {
    part.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn make_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
    }
}

fn make_exclusive(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

fn random_suffix() -> String {
    bot_core::random::random_hex(8)
}

impl CallDir {
    fn create(workspace: &Path, attempt: &Attempt) -> io::Result<CallDir> {
        Self::create_with(workspace, attempt, random_suffix)
    }

    fn create_with(
        workspace: &Path,
        attempt: &Attempt,
        mut suffix: impl FnMut() -> String,
    ) -> io::Result<CallDir> {
        make_dir(workspace)?;
        for _ in 0..16 {
            let name = format!(
                "{}-{}-g{}-c{}-{}",
                safe(&attempt.profile.0),
                safe(&attempt.run.0),
                attempt.generation.0,
                attempt.claim.map_or(0, |c| c.0),
                suffix()
            );
            let root = workspace.join(name);
            match make_exclusive(&root) {
                Ok(()) => {
                    let (input, work) = (root.join("in"), root.join("work"));
                    let dir = CallDir { root, input, work };
                    make_exclusive(&dir.input)?;
                    make_exclusive(&dir.work)?;
                    return Ok(dir);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::from(io::ErrorKind::AlreadyExists))
    }

    pub(crate) fn write_input(&self, relative: &str, content: &str) -> io::Result<()> {
        write_under(&self.input, relative, content)
    }

    pub(crate) fn write_work(&self, relative: &str, content: &str) -> io::Result<()> {
        write_under(&self.work, relative, content)
    }

    #[cfg(unix)]
    pub(crate) fn symlink_work(&self, relative: &str, target: &str) -> io::Result<()> {
        let path = self.work.join(relative);
        if let Some(parent) = path.parent() {
            make_dir(parent)?;
        }
        std::os::unix::fs::symlink(target, path)
    }
}

pub(crate) fn session_error(error: SessionError) -> ModelError {
    match error {
        SessionError::Lost => ModelError::Lost,
        SessionError::Unavailable => ModelError::Failed {
            code: "session_unavailable",
            transient: true,
        },
        SessionError::Permanent(code) => failed(code),
    }
}

pub(crate) fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn write_under(base: &Path, relative: &str, content: &str) -> io::Result<()> {
    let path = base.join(relative);
    if let Some(parent) = path.parent() {
        make_dir(parent)?;
    }
    fs::write(path, content)
}

impl Drop for CallDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

pub(crate) fn prepare_failed() -> ModelError {
    ModelError::Failed {
        code: "prepare_failed",
        transient: true,
    }
}

pub(crate) fn failed(code: &'static str) -> ModelError {
    ModelError::Failed {
        code,
        transient: false,
    }
}

pub(crate) fn prepare(
    config: &CliConfig,
    request: &ModelRequest,
) -> Result<(CliModel, CallDir, RelayChannel), ModelError> {
    let relay = request.relay.clone().ok_or(ModelError::Failed {
        code: "relay_unavailable",
        transient: true,
    })?;
    relay.validate()?;
    if !request.builtin.is_empty() && !config.confined_reads {
        return Err(failed("builtin_unconfined"));
    }
    let model = config
        .models
        .get(&request.model)
        .cloned()
        .ok_or(ModelError::Failed {
            code: "unknown_model",
            transient: false,
        })?;
    let call =
        CallDir::create(&config.workspace, &request.attempt).map_err(|_| prepare_failed())?;
    call.write_input("AGENTS.md", &request.system)
        .map_err(|_| prepare_failed())?;
    Ok((model, call, relay))
}

pub(crate) fn final_step(output: Output) -> ModelStep {
    ModelStep::Final {
        text: output.text,
        usage: output.usage,
        outcome: output.outcome,
    }
}

#[cfg(test)]
mod tests;
