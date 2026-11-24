//! SSE 중계. 상류 조각을 이벤트(빈 줄로 끝나는 덩어리) 단위로 모아 검사한 뒤 내보낸다.
//!
//! 검사는 이벤트를 내보내기 전에 끝난다:
//! - 원문: 조각·이벤트 경계를 넘는 겹침 버퍼(마지막 `키 길이 - 1`바이트)까지 포함해 키 바이트를 찾는다.
//! - 디코딩: `data:` 줄의 JSON을 풀어 키 이름과 문자열 값을 검사한다. 모델이 키를 델타 여러 개로 쪼개 흘려도
//!   같은 JSON 경로(`delta.text` 등)의 값을 이어 붙인 롤링 창으로 잡는다.
//!
//! 키의 앞부분: 어떤 창이든 끝이 키의 앞부분(`MIN_HOLD`바이트 이상, 키 전체는 아님)과 같으면 그 이벤트와 뒤따르는
//! 이벤트를 내보내지 않고 쥔다. 다음 이벤트가 키를 완성하면 쥔 이벤트까지 모두 버리고(앞부분도 나가지 않는다),
//! 모든 창이 앞부분으로 끝나지 않게 되면 쥔 이벤트를 순서대로 내보낸다. 정상 종결 이벤트(`message_stop`)가 와야만
//! 남은 쥔 이벤트를 내보내고 스트림을 끝낸다. 그 없이 연결만 끝나거나(조기 EOF) 오류·시간 초과로 끝나면 쥔 이벤트는 버리고
//! 고정 오류 이벤트로 끝낸다(앞부분이 오류와 함께 나가지 않는다).
//!
//! 도중에 막히거나(키 노출·크기·형식) 상류가 끊기거나 시간이 지나면 고정 어휘의 오류 이벤트 하나를 내보내고 끝낸다.
//! 상류 오류 상태(비 2xx)는 상태만 담은 고정 본문 하나로 바꾼다.

use super::ModelLimits;
use crate::{guard::contains_bytes, json::parse, rate::Slot};
use bot_core::port::Reply;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use tokio::time::{Instant, timeout};

/// 상류 본문을 조각 단위로 내주는 원천.
pub trait ChunkSource: Send {
    /// 다음 조각. 끝이면 `None`.
    fn next(&mut self) -> Reply<'_, Option<Vec<u8>>>;
}

pub struct UpstreamStream {
    pub status: u16,
    pub body: Box<dyn ChunkSource>,
}

/// 경로별 롤링 창의 최대 개수. 넘으면 한 창을 함께 쓴다.
const MAX_PATHS: usize = 256;

/// 창 끝이 키의 앞부분과 이만큼(바이트) 이상 같으면 이벤트를 쥔다. 일반 글에서도 한두 글자는 키의 첫머리와
/// 곧잘 겹쳐 스트림만 끊기고, 그만큼은 알려져도 쓸 수 없다. 키가 더 짧으면 `키 길이 - 1`까지 낮춘다.
const MIN_HOLD: usize = 3;

/// 이벤트 하나를 본 결과.
#[derive(Debug, Eq, PartialEq)]
enum Verdict {
    /// 키가 나타났다(또는 읽을 수 없다). 스트림을 끝낸다.
    Block,
    /// 키가 아직 없지만 어떤 창이 키의 앞부분으로 끝난다. 다음 이벤트가 정할 때까지 쥔다.
    Hold,
    /// 쥔 이벤트까지 내보내도 된다.
    Clear,
}

struct Scanner {
    key: Vec<u8>,
    /// 지금까지 본 바이트의 마지막 `키 길이 - 1`바이트.
    tail: Vec<u8>,
    windows: HashMap<String, Vec<u8>>,
    /// 지금 키의 앞부분으로 끝나는 창(경로 칸). 원문 꼬리는 빈 이름 대신 `RAW`로 센다.
    stuck: HashSet<String>,
}

/// 원문 꼬리 창의 이름(경로 이름과 겹치지 않는다: 경로는 `.`이나 `[]`로 시작하거나 빈 문자열이다).
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

    /// 창 끝이 키의 앞부분(`MIN_HOLD`바이트 이상, 키 전체는 아님)과 같은가.
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

    /// 이벤트 하나를 내보내도 되는가.
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
                // JSON이 아닌 데이터(`[DONE]` 등)도 같은 창으로 이어 본다.
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

    /// 같은 경로의 이전 값과 이어 붙여 키가 들어 있는지 본다.
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

/// 모델 샌드박스로 흘려 보낼 검사 끝난 이벤트의 원천. 오류 상태는 고정 본문 하나다.
pub struct ModelStream {
    status: u16,
    source: Option<Box<dyn ChunkSource>>,
    pending: Vec<u8>,
    /// 검사는 마쳤지만 키의 앞부분으로 끝나는 창 때문에 아직 내보내지 않은 이벤트.
    held: Vec<Vec<u8>>,
    /// 내보내도 되는 이벤트(앞에서부터).
    ready: VecDeque<Vec<u8>>,
    scanner: Scanner,
    limits: ModelLimits,
    started: Instant,
    total: usize,
    fixed: Option<Vec<u8>>,
    ended: bool,
    _slot: Slot,
}

/// 정상 종결 이벤트인가: 이벤트 이름과 `data:` JSON의 최상위 `type`이 모두 `message_stop`이고, 이름 줄과 데이터 줄이 하나씩뿐이다
/// (글 안의 같은 문자열과는 구별된다). 프런트의 응답 관측(`front::observe`)과 같은 규칙이라, 한쪽은 끝났다고 보는데 다른 쪽은
/// 아니라고 보는 이벤트가 없다.
fn is_message_stop(event: &[u8]) -> bool {
    if !contains_bytes(event, b"message_stop") {
        return false;
    }
    let Ok(text) = std::str::from_utf8(event) else {
        return false;
    };
    let (mut names, mut datas) = (Vec::new(), Vec::new());
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            names.push(rest.strip_prefix(' ').unwrap_or(rest));
        } else if let Some(rest) = line.strip_prefix("data:") {
            datas.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    matches!((names.as_slice(), datas.as_slice()), ([name], [data])
    if *name == "message_stop"
        && parse(data).is_some_and(|value| {
            value.get("type") == Some(&Value::String("message_stop".into()))
        }))
}

/// 이벤트 끝(`\n\n` 또는 `\r\n\r\n`) 바로 뒤 위치.
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

/// 고정 어휘 오류 이벤트(Anthropic 형식). 상류가 준 문장은 싣지 않는다.
/// 오류 종류는 클라이언트의 재시도 여부를 정한다: 키 노출·크기 초과는 같은 요청을 다시 보내도 같으므로
/// 재시도하지 않는 종류(`permission_error`·`invalid_request_error`)로, 상류 장애·시간 초과는 재시도하는 종류(`api_error`)로 알린다.
pub(crate) fn error_event(class: &str) -> Vec<u8> {
    let kind = match class {
        "blocked" | "stale" => "permission_error",
        "too_large" => "invalid_request_error",
        _ => "api_error",
    };
    format!("event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"{kind}\",\"message\":\"{class}\"}}}}\n\n").into_bytes()
}

impl ModelStream {
    pub(super) fn new(
        upstream: UpstreamStream,
        key: Vec<u8>,
        limits: ModelLimits,
        slot: Slot,
    ) -> Self {
        let failed = !(200..300).contains(&upstream.status);
        Self {
            status: upstream.status,
            fixed: failed
                .then(|| format!("{{\"error\":{{\"status\":{}}}}}", upstream.status).into_bytes()),
            source: (!failed).then_some(upstream.body),
            pending: Vec::new(),
            held: Vec::new(),
            ready: VecDeque::new(),
            scanner: Scanner::new(key),
            limits,
            started: Instant::now(),
            total: 0,
            ended: false,
            _slot: slot,
        }
    }

    /// 상류가 준 HTTP 상태. 2xx가 아니면 `next`가 고정 본문 하나만 내준다.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// 오류 이벤트 하나로 끝낸다. 쥐고 있던 이벤트는 버린다(키의 앞부분으로 끝나는 내용이 오류와 함께 나가지 않게).
    fn fail(&mut self, class: &str) -> Option<Vec<u8>> {
        self.ended = true;
        self.source = None;
        self.held.clear();
        Some(error_event(class))
    }

    /// 검사를 마친 다음 이벤트(또는 고정 오류 이벤트). 끝나면 `None`.
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
                // 완성된 이벤트도 크기 한도를 넘으면 검사 전에 끝낸다.
                if end > self.limits.max_event {
                    return self.fail("too_large");
                }
                let event: Vec<u8> = self.pending.drain(..end).collect();
                let verdict = self.scanner.check(&event);
                if verdict == Verdict::Block {
                    return self.fail("blocked");
                }
                let stop = is_message_stop(&event);
                if verdict == Verdict::Hold && !stop {
                    self.held.push(event);
                } else {
                    self.ready.extend(self.held.drain(..));
                    self.ready.push_back(event);
                }
                if stop {
                    // 정상 종결: 쥔 이벤트까지 내보내고 끝낸다. 그 뒤에 오는 것은 프로토콜 밖이라 받지 않는다.
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
                // 정상 응답은 `message_stop`으로만 끝난다(그때 `source`를 닫아 여기에 오지 않는다). 그 밖의 EOF는 잘린 스트림이다.
                Ok(Ok(None)) => return self.fail("truncated"),
                Ok(Err(_)) => return self.fail("unavailable"),
                Err(_) => return self.fail("timeout"),
            }
        }
    }
}

#[cfg(test)]
mod tests;
