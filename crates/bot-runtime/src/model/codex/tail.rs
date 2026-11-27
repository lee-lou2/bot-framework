//! Incremental Codex rollout JSONL tailer verifying broker confirmations and committing checkpoints.

use super::{
    open::{Discovery, Opened, discover_rollout, open_beneath},
    session::{Journal, Plan, Refused, Why},
};
use crate::model::{
    ModelError,
    driver::SessionWatcher,
    session::{MAX_LINE_BYTES, Session, SessionError},
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    future::Future,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    pin::Pin,
    time::{Duration, Instant},
};

const READ_CHUNK: usize = 4 << 20;
const CONFIRM_WITHIN: Duration = Duration::from_secs(30);
const PROBE_EVERY: Duration = Duration::from_millis(40);
const FINISH_CHUNKS: usize = 16;

enum Pull {
    Progress,
    Eof,
    Failed,
}

fn failed(code: &'static str, transient: bool) -> ModelError {
    ModelError::Failed { code, transient }
}

fn from_port(error: SessionError) -> Option<ModelError> {
    match error {
        SessionError::Lost => Some(ModelError::Lost),
        SessionError::Permanent(code) => Some(failed(code, false)),
        SessionError::Unavailable => None,
    }
}

pub(crate) struct CodexTail {
    root: PathBuf,
    relative: Option<String>,
    offset: u64,
    partial: Vec<u8>,
    skipping: bool,
    journal: Journal,
    port: Session,
    since: BTreeMap<String, Instant>,
    probed: Option<Instant>,
}

impl CodexTail {
    pub(crate) fn new(
        root: PathBuf,
        relative: Option<String>,
        offset: u64,
        journal: Journal,
        port: Session,
    ) -> Self {
        Self {
            root,
            relative,
            offset,
            partial: Vec::new(),
            skipping: false,
            journal,
            port,
            since: BTreeMap::new(),
            probed: None,
        }
    }

    fn resolve_path(&mut self) -> Result<Option<&str>, ModelError> {
        if self.relative.is_none() {
            match discover_rollout(&self.root) {
                Discovery::Found(rel) => self.relative = Some(rel),
                Discovery::Missing => return Ok(None),
                Discovery::Unsafe => return Err(failed("session_unsafe", false)),
            }
        }
        Ok(self.relative.as_deref())
    }

    fn read_new(&mut self) -> Result<Pull, ModelError> {
        let Some(relative) = self.resolve_path()?.map(str::to_owned) else {
            return Ok(Pull::Eof);
        };
        let mut file = match open_beneath(&self.root, &relative) {
            Opened::File(file) => file,
            Opened::Missing => return Ok(Pull::Eof),
            Opened::Unsafe => return Err(failed("session_unsafe", false)),
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Ok(Pull::Failed);
        }
        let mut chunk = Vec::new();
        let Ok(read) = file.take(READ_CHUNK as u64).read_to_end(&mut chunk) else {
            return Ok(Pull::Failed);
        };
        self.offset += read as u64;
        self.partial.extend_from_slice(&chunk);
        while let Some(end) = self.partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=end).collect();
            if std::mem::take(&mut self.skipping) {
                continue;
            }
            if let Ok(text) = std::str::from_utf8(&line[..line.len() - 1])
                && !text.trim().is_empty()
            {
                self.journal.feed(text);
            }
        }
        if self.partial.len() > MAX_LINE_BYTES {
            self.journal.freeze(Why::TooLarge);
            self.partial.clear();
            self.skipping = true;
        } else if self.skipping {
            self.partial.clear();
        }
        Ok(if read == 0 { Pull::Eof } else { Pull::Progress })
    }

    async fn write(&mut self, plan: Plan) -> Result<bool, ModelError> {
        for slot in &plan.verify {
            match self.port.0.recall(slot).await {
                Ok(Some(_)) => {}
                Ok(None) => {
                    self.journal.freeze(Why::Unsaved);
                    return Ok(true);
                }
                Err(error) => return from_port(error).map_or(Ok(false), Err),
            }
        }
        match self.port.0.commit(plan.steps.clone()).await {
            Ok(()) => {
                self.journal.applied(&plan);
                Ok(true)
            }
            Err(error) => from_port(error).map_or(Ok(false), Err),
        }
    }

    fn last_record(&mut self) -> Result<(), ModelError> {
        if self.skipping || self.partial.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let text = std::str::from_utf8(&self.partial)
            .ok()
            .filter(|t| serde_json::from_str::<Value>(t).is_ok())
            .map(str::to_owned)
            .ok_or_else(|| failed("session_unsaved", true))?;
        self.partial.clear();
        self.journal.feed(&text);
        Ok(())
    }

    fn stuck(&self) -> Result<(), ModelError> {
        if self.journal.malformed() {
            return Err(failed("session_mismatch", false));
        }
        match self.journal.stuck() {
            Some(Why::Unsaved) => Err(failed("session_unsaved", true)),
            Some(Why::TooLarge) => Err(failed("session_too_large", false)),
            None => Ok(()),
        }
    }

    async fn pump(&mut self) -> Result<(), ModelError> {
        self.read_new()?;
        if self.journal.malformed() {
            return Err(failed("session_mismatch", false));
        }
        while let Some(id) = self.journal.oldest().map(str::to_owned) {
            let first = *self.since.entry(id.clone()).or_insert_with(Instant::now);
            if self.probed.is_some_and(|at| at.elapsed() < PROBE_EVERY) {
                return Ok(());
            }
            self.probed = Some(Instant::now());
            let waiting = match self.port.0.response(&id).await {
                Ok(Some(response)) => match self.journal.plan(&response) {
                    Ok(plan) => {
                        if self.write(plan).await? {
                            self.since.remove(&id);
                            self.probed = None;
                            continue;
                        }
                        true
                    }
                    Err(Refused::Incomplete | Refused::NotYet) => true,
                    Err(Refused::Mismatch | Refused::Unverified) => {
                        return Err(failed("session_mismatch", false));
                    }
                },
                Ok(None) => true,
                Err(error) => from_port(error).map_or(Ok(true), Err)?,
            };
            if waiting && first.elapsed() > CONFIRM_WITHIN {
                return Err(failed("response_unconfirmed", true));
            }
            return Ok(());
        }
        Ok(())
    }

    async fn poll_inner(&mut self) -> Result<(), ModelError> {
        self.pump().await?;
        self.stuck()
    }

    async fn finish_inner(&mut self) -> Result<(), ModelError> {
        let mut drained = false;
        for _ in 0..=FINISH_CHUNKS {
            match self.read_new()? {
                Pull::Progress => tokio::task::yield_now().await,
                Pull::Eof => {
                    drained = true;
                    break;
                }
                Pull::Failed => return Err(failed("session_unreadable", true)),
            }
        }
        if !drained {
            return Err(failed("session_too_large", false));
        }
        self.last_record()?;
        self.stuck()?;
        if self.journal.unsaved_relay() {
            return Err(failed("session_unsaved", true));
        }
        for slot in self.journal.saved_relay() {
            match self.port.0.ended(slot).await {
                Ok(true) => {}
                Ok(false) => return Err(failed("session_unsaved", true)),
                Err(error) => {
                    return Err(from_port(error).unwrap_or(failed("session_unavailable", true)));
                }
            }
        }
        Ok(())
    }
}

impl SessionWatcher for CodexTail {
    fn poll(&mut self) -> Pin<Box<dyn Future<Output = Result<(), ModelError>> + Send + '_>> {
        Box::pin(self.poll_inner())
    }

    fn finish(&mut self) -> Pin<Box<dyn Future<Output = Result<(), ModelError>> + Send + '_>> {
        Box::pin(self.finish_inner())
    }
}
