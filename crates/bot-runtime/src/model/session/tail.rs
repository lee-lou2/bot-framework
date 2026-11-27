//! Incremental JSONL session file tailer that verifies relay confirmations and commits checkpoint batches.

use super::{
    Journal, MAX_LINE_BYTES, Plan, Refused, Session, SessionError, Why,
    open::{Opened, open_beneath},
};
use crate::model::ModelError;
use std::{
    collections::BTreeMap,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
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

pub struct Tail {
    root: PathBuf,
    relative: String,
    offset: u64,
    partial: Vec<u8>,
    skipping: bool,
    journal: Journal,
    port: Session,
    since: BTreeMap<String, Instant>,
    probed: Option<Instant>,
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

impl Tail {
    pub fn new(
        root: PathBuf,
        relative: String,
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

    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    fn read_new(&mut self) -> Result<Pull, ModelError> {
        let mut file = match open_beneath(&self.root, &self.relative) {
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
        // Freeze the journal and discard the remainder if an unterminated line exceeds `MAX_LINE_BYTES`.
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
                // Placeholder tool result whose call never ended in the ledger: freeze so resume settles it.
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

    /// Feeds a final unterminated JSON record if complete, or fails closed if truncated.
    fn last_record(&mut self) -> Result<(), ModelError> {
        if self.skipping || self.partial.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        let text = std::str::from_utf8(&self.partial)
            .ok()
            .filter(|t| serde_json::from_str::<serde_json::Value>(t).is_ok())
            .map(str::to_owned)
            .ok_or_else(|| failed("session_unsaved", true))?;
        self.partial.clear();
        self.journal.feed(&text);
        Ok(())
    }

    fn stuck(&self) -> Result<(), ModelError> {
        match self.journal.stuck() {
            Some(Why::Unsaved) => Err(failed("session_unsaved", true)),
            Some(Why::TooLarge) => Err(failed("session_too_large", false)),
            None => Ok(()),
        }
    }

    pub async fn poll(&mut self) -> Result<(), ModelError> {
        self.pump().await?;
        self.stuck()
    }

    /// Drains the session file after CLI exit and verifies every saved relay call completed in the ledger.
    pub async fn finish(&mut self) -> Result<(), ModelError> {
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

    async fn pump(&mut self) -> Result<(), ModelError> {
        self.read_new()?;
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
}
