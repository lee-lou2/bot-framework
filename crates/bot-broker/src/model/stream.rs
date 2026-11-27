//! SSE model response scanner that buffers and vets events for leaked API keys before emission.
//!
//! - Scans both raw bytes across chunk/event boundaries (keeping the last `key.len() - 1` bytes) and decoded JSON string values per JSON path across split deltas.
//! - Holds events while any rolling window ends with a key prefix (`>= MIN_HOLD` bytes); drops held events on key completion, early EOF, error, or timeout, and releases them only once all windows clear or on valid `message_stop`.
//! - Non-2xx upstream responses collapse to a single fixed status-only body; mid-stream failures emit one fixed-vocabulary Anthropic SSE error event.

use super::{ModelLimits, Protocol};
use crate::{guard::contains_bytes, json::parse, rate::Slot};
use bot_core::port::Reply;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use tokio::time::{Instant, timeout};

pub trait ChunkSource: Send {
    fn next(&mut self) -> Reply<'_, Option<Vec<u8>>>;
}

pub struct UpstreamStream {
    pub status: u16,
    pub body: Box<dyn ChunkSource>,
}

/// Maximum distinct JSON path rolling windows before overflowing into a shared fallback slot.
const MAX_PATHS: usize = 256;

/// Minimum trailing prefix match length (bytes) that triggers event holdback; avoids stuttering on 1-2 byte coincidences.
const MIN_HOLD: usize = 3;

#[derive(Debug, Eq, PartialEq)]
enum Verdict {
    Block,
    Hold,
    Clear,
}

struct Scanner {
    key: Vec<u8>,
    tail: Vec<u8>,
    windows: HashMap<String, Vec<u8>>,
    /// Slots currently ending in a key prefix; raw byte tail uses `RAW`.
    stuck: HashSet<String>,
}

/// Disjoint key for the raw tail window (JSON paths start with `.`, `[]`, or `""`).
const RAW: &str = "\0raw";

impl Scanner {
    fn new(key: Vec<u8>) -> Self {
        Self {
            key,
            tail: Vec::new(),
            windows: HashMap::new(),
            stuck: HashSet::new(),
        }
    }

    fn keep(&self, window: &[u8]) -> Vec<u8> {
        let keep = self.key.len().saturating_sub(1).min(window.len());
        window[window.len() - keep..].to_vec()
    }

    fn ends_in_key_prefix(&self, window: &[u8]) -> bool {
        let most = self.key.len().saturating_sub(1).min(window.len());
        let least = MIN_HOLD.min(self.key.len().saturating_sub(1)).max(1);
        (least..=most).any(|n| window.ends_with(&self.key[..n]))
    }

    fn mark(&mut self, slot: &str, window: &[u8]) {
        if self.ends_in_key_prefix(window) {
            self.stuck.insert(slot.to_owned());
        } else {
            self.stuck.remove(slot);
        }
    }

    fn check(&mut self, event: &[u8]) -> Verdict {
        if self.key.is_empty() {
            return Verdict::Clear;
        }
        let mut window = std::mem::take(&mut self.tail);
        window.extend_from_slice(event);
        let hit = contains_bytes(&window, &self.key);
        self.tail = self.keep(&window);
        let tail = self.tail.clone();
        self.mark(RAW, &tail);
        if hit {
            return Verdict::Block;
        }
        let Ok(text) = std::str::from_utf8(event) else {
            return Verdict::Block;
        };
        for line in text.lines() {
            let Some(rest) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = rest.strip_prefix(' ').unwrap_or(rest);
            let leaked = match parse(payload) {
                Some(value) => self.walk("", &value),
                None => self.roll("", payload),
            };
            if leaked {
                return Verdict::Block;
            }
        }
        if self.stuck.is_empty() {
            Verdict::Clear
        } else {
            Verdict::Hold
        }
    }

    fn walk(&mut self, path: &str, value: &Value) -> bool {
        match value {
            Value::String(text) => self.roll(path, text),
            Value::Array(items) => {
                let child = format!("{path}[]");
                items.iter().any(|item| self.walk(&child, item))
            }
            Value::Object(map) => map.iter().any(|(name, item)| {
                contains_bytes(name.as_bytes(), &self.key)
                    || self.walk(&format!("{path}.{name}"), item)
            }),
            _ => false,
        }
    }

    fn roll(&mut self, path: &str, text: &str) -> bool {
        let slot = if self.windows.len() < MAX_PATHS || self.windows.contains_key(path) {
            path
        } else {
            "*"
        };
        let mut window = self.windows.remove(slot).unwrap_or_default();
        window.extend_from_slice(text.as_bytes());
        let hit = contains_bytes(&window, &self.key);
        let kept = self.keep(&window);
        self.mark(slot, &kept);
        self.windows.insert(slot.to_owned(), kept);
        hit
    }
}

pub struct ModelStream {
    status: u16,
    protocol: Protocol,
    source: Option<Box<dyn ChunkSource>>,
    pending: Vec<u8>,
    held: Vec<Vec<u8>>,
    ready: VecDeque<Vec<u8>>,
    scanner: Scanner,
    limits: ModelLimits,
    started: Instant,
    total: usize,
    fixed: Option<Vec<u8>>,
    seen_finish: bool,
    ended: bool,
    _slot: Slot,
}

fn single_event_type(event: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(event).ok()?;
    let (mut names, mut datas) = (Vec::new(), Vec::new());
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            names.push(rest.strip_prefix(' ').unwrap_or(rest));
        } else if let Some(rest) = line.strip_prefix("data:") {
            datas.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    match (names.as_slice(), datas.as_slice()) {
        ([name], [data]) => {
            let value = parse(data)?;
            (value.get("type").and_then(Value::as_str) == Some(*name)).then(|| (*name).to_owned())
        }
        _ => None,
    }
}

/// Matches `front::observe`'s terminal SSE event rules so stream termination and turn observation always agree.
fn is_hard_stop(protocol: Protocol, event: &[u8]) -> bool {
    match protocol {
        Protocol::Anthropic => {
            contains_bytes(event, b"message_stop")
                && single_event_type(event).as_deref() == Some("message_stop")
        }
        Protocol::OpenAiResponses => {
            (contains_bytes(event, b"response.completed")
                || contains_bytes(event, b"response.incomplete")
                || contains_bytes(event, b"response.failed"))
                && matches!(
                    single_event_type(event).as_deref(),
                    Some("response.completed" | "response.incomplete" | "response.failed")
                )
        }
        Protocol::Gemini => false,
    }
}

fn gemini_has_finish(event: &[u8]) -> bool {
    if !contains_bytes(event, b"finishReason") {
        return false;
    }
    let Ok(text) = std::str::from_utf8(event) else {
        return false;
    };
    let mut data = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data:")
            && data
                .replace(rest.strip_prefix(' ').unwrap_or(rest))
                .is_some()
        {
            return false;
        }
    }
    data.and_then(parse).is_some_and(|value| {
        value
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
            .is_some_and(|reason| !reason.is_empty())
    })
}

fn event_end(buffer: &[u8]) -> Option<usize> {
    let lf = buffer.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Fixed-vocabulary protocol SSE error event; maps permanent failures to non-retryable error codes.
pub(crate) fn error_event(protocol: Protocol, class: &str) -> Vec<u8> {
    match protocol {
        Protocol::Anthropic => {
            let kind = match class {
                "blocked" | "stale" => "permission_error",
                "too_large" => "invalid_request_error",
                _ => "api_error",
            };
            format!(
                "event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"{kind}\",\"message\":\"{class}\"}}}}\n\n"
            )
            .into_bytes()
        }
        Protocol::OpenAiResponses => {
            let code = match class {
                "blocked" | "stale" => "permission_denied",
                "too_large" => "invalid_request",
                _ => "server_error",
            };
            format!(
                "event: error\ndata: {{\"type\":\"error\",\"code\":\"{code}\",\"message\":\"{class}\"}}\n\n"
            )
            .into_bytes()
        }
        Protocol::Gemini => {
            let (code, status) = match class {
                "blocked" | "stale" => (403, "PERMISSION_DENIED"),
                "too_large" => (400, "INVALID_ARGUMENT"),
                _ => (503, "UNAVAILABLE"),
            };
            format!(
                "data: {{\"error\":{{\"code\":{code},\"message\":\"{class}\",\"status\":\"{status}\"}}}}\n\n"
            )
            .into_bytes()
        }
    }
}

impl ModelStream {
    pub(super) fn new(
        upstream: UpstreamStream,
        key: Vec<u8>,
        limits: ModelLimits,
        slot: Slot,
        protocol: Protocol,
    ) -> Self {
        let failed = !(200..300).contains(&upstream.status);
        Self {
            status: upstream.status,
            protocol,
            fixed: failed
                .then(|| super::upstream_error_body(protocol, upstream.status).into_bytes()),
            source: (!failed).then_some(upstream.body),
            pending: Vec::new(),
            held: Vec::new(),
            ready: VecDeque::new(),
            scanner: Scanner::new(key),
            limits,
            started: Instant::now(),
            total: 0,
            seen_finish: false,
            ended: false,
            _slot: slot,
        }
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    /// Terminates with one fixed error event and discards any held prefix-matching events.
    fn fail(&mut self, class: &str) -> Option<Vec<u8>> {
        self.ended = true;
        self.source = None;
        self.held.clear();
        Some(error_event(self.protocol, class))
    }

    pub async fn next(&mut self) -> Option<Vec<u8>> {
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Some(event);
            }
            if self.ended {
                return None;
            }
            if let Some(body) = self.fixed.take() {
                self.ended = true;
                return Some(body);
            }
            if let Some(end) = event_end(&self.pending) {
                if end > self.limits.max_event {
                    return self.fail("too_large");
                }
                let event: Vec<u8> = self.pending.drain(..end).collect();
                let verdict = self.scanner.check(&event);
                if verdict == Verdict::Block {
                    return self.fail("blocked");
                }
                let stop = is_hard_stop(self.protocol, &event);
                if self.protocol == Protocol::Gemini && gemini_has_finish(&event) {
                    self.seen_finish = true;
                }
                // Keep holding key-prefix chunks until hard stop or clean Gemini EOF so trailing chunks cannot complete a leaked key.
                if verdict == Verdict::Hold && !stop {
                    self.held.push(event);
                } else {
                    self.ready.extend(self.held.drain(..));
                    self.ready.push_back(event);
                }
                if stop {
                    // Close upstream on terminal event; anything trailing is out-of-protocol.
                    self.ended = true;
                    self.source = None;
                }
                continue;
            }
            if self.pending.len() > self.limits.max_event {
                return self.fail("too_large");
            }
            let remaining = self
                .limits
                .stream_total
                .saturating_sub(self.started.elapsed());
            if remaining.is_zero() {
                return self.fail("timeout");
            }
            let wait = self.limits.stream_idle.min(remaining);
            let Some(source) = self.source.as_mut() else {
                self.ended = true;
                return None;
            };
            match timeout(wait, source.next()).await {
                Ok(Ok(Some(chunk))) => {
                    self.total += chunk.len();
                    if self.total > self.limits.max_stream {
                        return self.fail("too_large");
                    }
                    self.pending.extend_from_slice(&chunk);
                }
                Ok(Ok(None)) => {
                    if self.protocol == Protocol::Gemini
                        && self.seen_finish
                        && self.pending.iter().all(|b| b.is_ascii_whitespace())
                    {
                        self.ended = true;
                        self.source = None;
                        self.ready.extend(self.held.drain(..));
                        continue;
                    }
                    return self.fail("truncated");
                }
                Ok(Err(_)) => return self.fail("unavailable"),
                Err(_) => return self.fail("timeout"),
            }
        }
    }
}

#[cfg(test)]
mod tests;
