//! Effect ledger callback channel (`effect.request` and `effect.status`) for multi-step tools.
//!
//! Each UTF-8 NDJSON line (64 KiB max) carries one request or response. Attempt, call, and tool
//! identity are fixed by the channel rather than tool frames. `effect.status` supports an effect-ID
//! form (read/record) and a read-only lookup form (`step` + `args`, `send: false`); mixing forms or
//! supplying operator-only `resolution` is rejected as `invalid`. Credentials detected in reported
//! receipts or reasons cause the step to be held under fixed reason codes without storing raw text.

use crate::{
    canon::canonicalize,
    guard::blocks_output,
    json::parse,
    lines::read_line,
    sandbox::{BoxRead, BoxWrite},
    text::json_quote,
};
use bot_core::{
    command::*,
    id::*,
    port::{EffectJournal, Error},
    state::{EffectResolution, EffectState},
};
use serde_json::{Map, Value};
use std::sync::Arc;
use tokio::{
    io::{AsyncWriteExt, BufReader},
    sync::watch,
};

pub const MAX_FRAME: usize = 64 * 1024;

/// Hold reason when a confirmed receipt leaks credentials and raw text is discarded.
pub const RECEIPT_CREDENTIAL: &str = "receipt_credential";
/// Hold reason when a held reason leaks credentials and raw text is discarded.
pub const REASON_CREDENTIAL: &str = "reason_credential";

pub struct EffectChannel {
    journal: Arc<dyn EffectJournal>,
    attempt: Attempt,
    call: CallId,
    /// Verified by the journal on lookup; never taken from tool frames.
    tool: ToolId,
    /// Declared secret bytes and proxy password checked against tool-reported text before storage.
    credentials: Vec<Vec<u8>>,
}

fn state_name(state: EffectState) -> &'static str {
    match state {
        EffectState::Started => "started",
        EffectState::Confirmed => "confirmed",
        EffectState::Rejected => "rejected",
        EffectState::Held => "held",
    }
}

fn error_name(error: &Error) -> &'static str {
    match error {
        Error::Denied => "denied",
        Error::Conflict => "conflict",
        Error::Stale => "stale",
        Error::Unavailable | Error::RateLimited => "unavailable",
        Error::Invalid | Error::Missing => "invalid",
    }
}

fn failure(id: Option<&str>, class: &str) -> String {
    match id {
        Some(id) => format!(
            "{{\"v\":1,\"id\":{},\"error\":{}}}",
            json_quote(id),
            json_quote(class)
        ),
        None => format!("{{\"v\":1,\"error\":{}}}", json_quote(class)),
    }
}

fn answer(id: &str, status: &EffectStatus, send: bool, with_effect: bool) -> String {
    let mut out = format!(
        "{{\"v\":1,\"id\":{},\"state\":{}",
        json_quote(id),
        json_quote(state_name(status.state))
    );
    if with_effect {
        out.push_str(&format!(",\"effect\":{}", json_quote(&status.effect.0)));
    }
    if let Some(receipt) = &status.receipt {
        out.push_str(&format!(",\"receipt\":{}", json_quote(receipt)));
    }
    out.push_str(&format!(",\"send\":{send}}}"));
    out
}

/// Lookup response (`send` is always `false`): receipts appear only on `confirmed`, and
/// `resolution: "not_executed"` appears only on operator-resolved `held` steps.
fn lookup_answer(id: &str, status: &EffectStatus) -> String {
    let mut out = format!(
        "{{\"v\":1,\"id\":{},\"state\":{},\"effect\":{}",
        json_quote(id),
        json_quote(state_name(status.state)),
        json_quote(&status.effect.0)
    );
    if status.state == EffectState::Confirmed
        && let Some(receipt) = &status.receipt
    {
        out.push_str(&format!(",\"receipt\":{}", json_quote(receipt)));
    }
    if status.state == EffectState::Held && status.resolution == Some(EffectResolution::NotExecuted)
    {
        out.push_str(",\"resolution\":\"not_executed\"");
    }
    out.push_str(",\"send\":false}");
    out
}

fn valid_step(step: &str) -> bool {
    let mut bytes = step.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_lowercase())
        && step.len() <= 63
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn text<'a>(frame: &'a Map<String, Value>, key: &str, max: usize) -> Option<&'a str> {
    frame
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= max)
}

impl EffectChannel {
    pub fn new(
        journal: Arc<dyn EffectJournal>,
        attempt: Attempt,
        call: CallId,
        tool: ToolId,
    ) -> Self {
        Self {
            journal,
            attempt,
            call,
            tool,
            credentials: Vec::new(),
        }
    }

    pub fn with_credentials(mut self, credentials: Vec<Vec<u8>>) -> Self {
        self.credentials = credentials;
        self
    }

    fn leaks(&self, text: &str) -> bool {
        blocks_output(text, &self.credentials)
    }

    pub async fn handle(&self, line: &[u8]) -> String {
        let Some(Value::Object(frame)) = std::str::from_utf8(line).ok().and_then(parse) else {
            return failure(None, "invalid");
        };
        let Some(id) = text(&frame, "id", 64) else {
            return failure(None, "invalid");
        };
        if frame.get("v").and_then(Value::as_u64) != Some(1) {
            return failure(Some(id), "invalid");
        }
        // `resolution` is operator-only and must never be accepted from tool frames.
        if frame.contains_key("resolution") {
            return failure(Some(id), "invalid");
        }
        let result = match frame.get("method").and_then(Value::as_str) {
            Some("effect.request") => self.request(id, &frame).await,
            Some("effect.status") => self.status(id, &frame).await,
            _ => Err(Error::Invalid),
        };
        result.unwrap_or_else(|error| failure(Some(id), error_name(&error)))
    }

    async fn request(&self, id: &str, frame: &Map<String, Value>) -> Result<String, Error> {
        let step = text(frame, "step", 63)
            .filter(|s| valid_step(s) && !self.leaks(s))
            .ok_or(Error::Invalid)?;
        let args = frame
            .get("args")
            .filter(|a| a.is_object())
            .ok_or(Error::Invalid)?;
        let args = JsonPayload(canonicalize(args).ok_or(Error::Invalid)?);
        let permit = self
            .journal
            .request(EffectRequest {
                attempt: self.attempt.clone(),
                call: self.call.clone(),
                step: step.to_owned(),
                args,
            })
            .await?;
        Ok(answer(id, &permit.status, permit.send, true))
    }

    /// Read-only lookup form (`step` + `args`); rejects frames mixing effect-ID/outcome keys.
    async fn lookup(&self, id: &str, frame: &Map<String, Value>) -> Result<String, Error> {
        if ["effect", "outcome", "receipt", "reason"]
            .iter()
            .any(|key| frame.contains_key(*key))
        {
            return Err(Error::Invalid);
        }
        let step = text(frame, "step", 63)
            .filter(|s| valid_step(s) && !self.leaks(s))
            .ok_or(Error::Invalid)?;
        let args = frame
            .get("args")
            .filter(|a| a.is_object())
            .ok_or(Error::Invalid)?;
        let args = JsonPayload(canonicalize(args).ok_or(Error::Invalid)?);
        let found = self
            .journal
            .lookup(
                EffectRequest {
                    attempt: self.attempt.clone(),
                    call: self.call.clone(),
                    step: step.to_owned(),
                    args,
                },
                self.tool.clone(),
            )
            .await?;
        Ok(match found {
            None => format!(
                "{{\"v\":1,\"id\":{},\"state\":\"none\",\"send\":false}}",
                json_quote(id)
            ),
            Some(status) => lookup_answer(id, &status),
        })
    }

    async fn status(&self, id: &str, frame: &Map<String, Value>) -> Result<String, Error> {
        if frame.contains_key("step") || frame.contains_key("args") {
            return self.lookup(id, frame).await;
        }
        let effect = EffectId(text(frame, "effect", 64).ok_or(Error::Invalid)?.to_owned());
        let Some(outcome) = frame.get("outcome") else {
            let status = self
                .journal
                .status(self.attempt.clone(), self.call.clone(), effect)
                .await?;
            return Ok(answer(id, &status, false, false));
        };
        let receipt = text(frame, "receipt", 512).map(str::to_owned);
        let reason = text(frame, "reason", 64)
            .filter(|r| {
                r.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            })
            .map(str::to_owned);
        let (state, receipt) = match outcome.as_str() {
            // Confirmed effects require a remote receipt so uncertain outcomes cannot be marked sent.
            Some("confirmed") => (EffectState::Confirmed, Some(receipt.ok_or(Error::Invalid)?)),
            Some("rejected") => (EffectState::Rejected, None),
            // Hold reason codes are stored in the ledger's receipt column.
            Some("held") => (EffectState::Held, Some(reason.ok_or(Error::Invalid)?)),
            _ => return Err(Error::Invalid),
        };
        // Screen for leaked credentials before writing to the ledger or audit trail.
        let (state, receipt) = match (state, receipt) {
            (EffectState::Confirmed, Some(text)) if self.leaks(&text) => {
                (EffectState::Held, Some(RECEIPT_CREDENTIAL.to_owned()))
            }
            (EffectState::Held, Some(text)) if self.leaks(&text) => {
                (EffectState::Held, Some(REASON_CREDENTIAL.to_owned()))
            }
            other => other,
        };
        let status = EffectStatus {
            effect,
            state,
            resolution: None,
            receipt,
        };
        self.journal
            .record(self.attempt.clone(), self.call.clone(), status.clone())
            .await?;
        Ok(answer(id, &status, false, false))
    }

    pub async fn serve(self, read: BoxRead, mut write: BoxWrite, mut stop: watch::Receiver<bool>) {
        let mut read = BufReader::new(read);
        loop {
            let line = tokio::select! {
                biased;
                _ = stop.changed() => return,
                line = read_line(&mut read, MAX_FRAME) => line,
            };
            let reply = match line {
                Ok(Some(line)) => self.handle(&line).await,
                Ok(None) => return,
                Err(_) => {
                    let _ = write
                        .write_all(format!("{}\n", failure(None, "invalid")).as_bytes())
                        .await;
                    return;
                }
            };
            if write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .is_err()
                || write.flush().await.is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod secret_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod verdict_tests;
