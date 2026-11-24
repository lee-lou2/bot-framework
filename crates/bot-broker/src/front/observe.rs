//! 완료한 모델 응답의 도구 목록 관측. 상류가 실제로 끝낸 응답(SSE는 `message_stop`, JSON은 본문 전체)에서 `tool_use`를
//! 순서대로 읽어 `TurnSink`로 알린다. 알리는 데 성공한 뒤에야 종결 이벤트·응답을 CLI에 내보낸다.
//!
//! 응답이 중간에 끊기거나 오류로 끝나면 아무것도 알리지 않는다(CLI는 오류를 받고 모델을 다시 부른다). 이 알림은 세션 기록(②)과
//! 대조해 그 차례의 도구가 실제로 몇 개·어떤 순서인지 확정하는 근거다: 대조가 끝난 그룹이 있기 전에는 도구를 실행하지 않는다.

use crate::json::parse;
use bot_core::{
    cli::{CliRefusal, CliResponse, CliResponseUse, RESPONSE_PREFIX},
    command::{JsonPayload, Step},
    id::{Attempt, CallId},
    port::{Error, Reply, RunStore},
};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

/// 한 응답 안의 도구 호출 하나. `input`은 모델이 만든 원 JSON이다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservedUse {
    pub id: String,
    pub name: String,
    pub input: String,
}

/// 완료한 응답. `id`는 제공자의 메시지 ID다. `name`은 제공자가 본 도구 이름 그대로다(예: `mcp__relay__agenda_delete`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Observed {
    pub id: String,
    pub uses: Vec<ObservedUse>,
}

/// 완료한 응답의 도구 목록을 받는 쪽. 쓰지 못하면 오류이고, 그러면 프런트는 응답을 내보내지 않는다.
pub trait TurnSink: Send + Sync {
    fn completed(&self, attempt: Attempt, turn: Observed) -> Reply<'_, ()>;

    /// 검증된 CLI 위치(`CallId::cli`)의 실행 전 확정 거절 표식을 run 체크포인트에 남긴다(`bot_core::cli::CliRefusal`). 쓰지 못하면 오류이고,
    /// 그러면 프런트는 거절 단어를 돌려주지 않는다. 기본은 아무것도 하지 않는다(재개를 쓰지 않는 구성).
    fn refused(&self, _: Attempt, _: CallId, _: CliRefusal) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// CLI 재개를 쓰지 않는 구성(시험·단순 호스트)용: 아무것도 하지 않는다.
pub struct NoSink;

impl TurnSink for NoSink {
    fn completed(&self, _: Attempt, _: Observed) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// 같은 run의 체크포인트에 `cli-resp-<id>`로 적는다(해당 claim으로, `bot_core::cli::CliResponse`). 같은 응답을 다시 알려도
/// 같은 내용이라 멱등이고, 같은 키에 다른 내용은 저장소가 거절한다.
pub struct CheckpointSink {
    runs: Arc<dyn RunStore>,
}

impl CheckpointSink {
    pub fn new(runs: Arc<dyn RunStore>) -> Self {
        Self { runs }
    }
}

/// 체크포인트 한 칸의 최대 크기(저장소 한도 1 MiB보다 조금 작게).
const MAX_STEP: usize = 900 << 10;

impl TurnSink for CheckpointSink {
    fn completed(&self, attempt: Attempt, turn: Observed) -> Reply<'_, ()> {
        Box::pin(async move {
            let mut uses = Vec::new();
            for u in &turn.uses {
                let input: Value = serde_json::from_str(&u.input).map_err(|_| Error::Invalid)?;
                uses.push(CliResponseUse {
                    id: u.id.clone(),
                    name: u.name.clone(),
                    input,
                });
            }
            let response = CliResponse { id: turn.id, uses };
            if !response.is_valid() {
                return Err(Error::Invalid);
            }
            let body = serde_json::to_string(&response).map_err(|_| Error::Invalid)?;
            if body.len() > MAX_STEP {
                return Err(Error::Invalid);
            }
            let step = Step {
                key: format!("{RESPONSE_PREFIX}{}", response.id),
                body: JsonPayload(body),
            };
            self.runs.checkpoint(attempt, step).await
        })
    }

    /// `cli-refusal-<호출 ID>`에 JSON 문자열 `"unknown_tool"`·`"invalid"`로 적는다. 같은 표식을 다시 써도 같은 내용이라 멱등이고,
    /// 같은 키에 다른 내용은 저장소가 거절한다.
    fn refused(&self, attempt: Attempt, call: CallId, refusal: CliRefusal) -> Reply<'_, ()> {
        Box::pin(async move {
            let body = serde_json::to_string(&refusal).map_err(|_| Error::Invalid)?;
            let step = Step {
                key: CliRefusal::key(&call),
                body: JsonPayload(body),
            };
            self.runs.checkpoint(attempt, step).await
        })
    }
}

/// 키·ID로 쓸 수 있는 글자인가.
fn id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 도구가 있는 응답은 제공자의 메시지 ID가 있어야 한다(없거나 못 쓰는 모양이면 다른 값으로 대신하지 않고 실패한다). 도구 사용 ID는
/// 한 응답 안에서 겹치지 않고, ID·이름은 비어 있지 않아야 한다. 도구가 없으면 `Ok(None)`.
fn observed(message: Option<String>, uses: Vec<ObservedUse>) -> Result<Option<Observed>, Error> {
    if uses.is_empty() {
        return Ok(None);
    }
    let id = message.filter(|id| id_ok(id)).ok_or(Error::Invalid)?;
    let mut seen = std::collections::BTreeSet::new();
    for u in &uses {
        if !id_ok(&u.id) || u.name.is_empty() || !seen.insert(u.id.as_str()) {
            return Err(Error::Invalid);
        }
    }
    Ok(Some(Observed { id, uses }))
}

/// 입력을 모으는 중인 도구 사용.
struct Open {
    id: String,
    name: String,
    json: String,
    /// 시작 이벤트가 이미 입력을 통째로 준 경우(비어 있지 않은 객체).
    initial: Option<Value>,
}

/// 응답 안의 블록 하나의 진행 상황. 블록 위치(`index`)는 응답 안에서 한 번만 쓰인다.
enum Block {
    /// 도구가 아닌 블록(글·생각 등): 시작과 끝만 센다.
    Other,
    /// 입력을 모으는 도구 사용.
    Tool(Open),
    /// 끝난 블록(도구 사용이면 그 내용).
    Closed(Option<ObservedUse>),
}

/// 한 도구 입력이 가질 수 있는 최대 크기(체크포인트 한 칸의 한도와 같다).
const MAX_INPUT: usize = 900 << 10;

/// 관측하는 이벤트 이름. 이름과 `data`의 `type`이 어긋난 이벤트는 CLI가 어떻게 읽을지 알 수 없다.
const TRACKED: [&str; 6] = [
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
];

/// SSE 이벤트를 하나씩 먹여 `tool_use`를 모은다. 계획이 CLI가 실제로 보는 도구와 다를 수 있는 모든 모양(같은 위치의 중복 시작·끝,
/// 시작 전·끝난 뒤의 델타, 객체가 아닌 입력, 이름과 종류가 어긋난 이벤트, 중복 키, 끝나지 않은 블록)은 응답 전체를 거절한다.
#[derive(Default)]
pub(super) struct Observer {
    message: Option<String>,
    started: bool,
    blocks: BTreeMap<u64, Block>,
    stopped: bool,
    broken: bool,
}

impl Observer {
    pub(super) fn feed(&mut self, event: &[u8]) {
        let Ok(text) = std::str::from_utf8(event) else {
            self.broken = true;
            return;
        };
        let (mut name, mut data) = (None, None);
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                // 이름이나 데이터가 둘 이상인 이벤트는 읽는 쪽마다 해석이 갈릴 수 있다.
                if name
                    .replace(rest.strip_prefix(' ').unwrap_or(rest))
                    .is_some()
                {
                    self.broken = true;
                    return;
                }
            } else if let Some(rest) = line.strip_prefix("data:")
                && data
                    .replace(rest.strip_prefix(' ').unwrap_or(rest))
                    .is_some()
            {
                self.broken = true;
                return;
            }
        }
        let Some(data) = data else {
            return; // 데이터 없는 이벤트(주석 등)
        };
        // 엄격한 JSON(중복 키·깊은 중첩 거절)만 읽는다. 읽을 수 없는 데이터가 낀 응답은 관측할 수 없다.
        let Some(value) = parse(data) else {
            self.broken = true;
            return;
        };
        self.event(name, &value);
    }

    fn event(&mut self, name: Option<&str>, value: &Value) {
        let kind = value.get("type").and_then(Value::as_str);
        let tracked = name.is_some_and(|n| TRACKED.contains(&n))
            || kind.is_some_and(|k| TRACKED.contains(&k));
        if tracked && name != kind {
            self.broken = true;
            return;
        }
        let index = value.get("index").and_then(Value::as_u64);
        match kind {
            Some("message_start") => {
                if self.started {
                    self.broken = true; // 둘째 시작
                    return;
                }
                self.started = true;
                self.message = value
                    .pointer("/message/id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("content_block_start") => self.start(index, value.get("content_block")),
            Some("content_block_delta") => self.delta(index, value.get("delta")),
            Some("content_block_stop") => self.stop(index),
            Some("message_stop") => self.stopped = true,
            _ => {}
        }
    }

    fn start(&mut self, index: Option<u64>, block: Option<&Value>) {
        let (Some(index), Some(block)) = (index, block) else {
            self.broken = true;
            return;
        };
        if self.blocks.contains_key(&index) {
            self.broken = true; // 같은 위치의 중복 시작은 덮어쓰지 않는다
            return;
        }
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            self.blocks.insert(index, Block::Other);
            return;
        }
        let (Some(id), Some(name)) = (
            block.get("id").and_then(Value::as_str),
            block.get("name").and_then(Value::as_str),
        ) else {
            self.broken = true;
            return;
        };
        // 시작 때 입력은 없거나 객체여야 한다(배열 같은 다른 값을 빈 객체로 바꿔 저장하지 않는다).
        let initial = match block.get("input") {
            None => None,
            Some(Value::Object(map)) if map.is_empty() => None,
            Some(input @ Value::Object(_)) => Some(input.clone()),
            Some(_) => {
                self.broken = true;
                return;
            }
        };
        self.blocks.insert(
            index,
            Block::Tool(Open {
                id: id.to_owned(),
                name: name.to_owned(),
                json: String::new(),
                initial,
            }),
        );
    }

    fn delta(&mut self, index: Option<u64>, delta: Option<&Value>) {
        let (Some(index), Some(delta)) = (index, delta) else {
            self.broken = true;
            return;
        };
        let json_delta = delta.get("type").and_then(Value::as_str) == Some("input_json_delta");
        match self.blocks.get_mut(&index) {
            // 시작 전이거나 이미 끝난 블록의 델타, 도구가 아닌 블록에 온 입력 조각
            None | Some(Block::Closed(_)) => self.broken = true,
            Some(Block::Other) => self.broken |= json_delta,
            Some(Block::Tool(open)) => {
                let part = delta.get("partial_json").and_then(Value::as_str);
                match part {
                    Some(part) if json_delta && open.json.len() + part.len() <= MAX_INPUT => {
                        open.json.push_str(part);
                    }
                    _ => self.broken = true,
                }
            }
        }
    }

    fn stop(&mut self, index: Option<u64>) {
        let Some(index) = index else {
            self.broken = true;
            return;
        };
        let closed = match self.blocks.remove(&index) {
            None | Some(Block::Closed(_)) => {
                self.broken = true; // 시작 없는 끝, 같은 위치의 중복 끝
                return;
            }
            Some(Block::Other) => None,
            Some(Block::Tool(open)) => match finish_input(&open) {
                Some(input) => Some(ObservedUse {
                    id: open.id,
                    name: open.name,
                    input,
                }),
                None => {
                    self.broken = true;
                    return;
                }
            },
        };
        self.blocks.insert(index, Block::Closed(closed));
    }

    pub(super) fn stopped(&self) -> bool {
        self.stopped
    }

    /// 계획이 CLI가 실제로 보는 것과 달라질 수 있는 이벤트를 봤다(그 이벤트는 내보내지 않고 응답을 고정 오류로 닫는다).
    pub(super) fn broken(&self) -> bool {
        self.broken
    }

    /// 종결 이벤트를 본 뒤: 도구 사용이 있으면 그 목록(블록 순서), 없으면 `None`. 읽을 수 없는 이벤트가 있었거나 끝나지 않은 블록이
    /// 남았거나 시작 이벤트가 없었으면 오류.
    pub(super) fn finish(&mut self) -> Result<Option<Observed>, Error> {
        let unfinished = self
            .blocks
            .values()
            .any(|block| !matches!(block, Block::Closed(_)));
        if self.broken || unfinished || !self.started {
            return Err(Error::Invalid);
        }
        let uses: Vec<ObservedUse> = std::mem::take(&mut self.blocks)
            .into_values()
            .filter_map(|block| match block {
                Block::Closed(Some(used)) => Some(used),
                _ => None,
            })
            .collect();
        observed(self.message.take(), uses)
    }
}

/// 도구 입력 하나를 확정한다: 시작 이벤트가 준 입력과 조각이 함께 있으면(어느 쪽이 맞는지 알 수 없다) 거절, 조각은 엄격한 JSON
/// 객체여야 한다. 입력이 없으면 빈 객체다.
fn finish_input(open: &Open) -> Option<String> {
    let fragments = !open.json.trim().is_empty();
    match (&open.initial, fragments) {
        (Some(_), true) => None,
        (Some(initial), false) => Some(initial.to_string()),
        (None, false) => Some("{}".to_owned()),
        (None, true) => match parse(&open.json)? {
            value @ Value::Object(_) => Some(value.to_string()),
            _ => None,
        },
    }
}

/// 스트리밍이 아닌 응답 본문(JSON)에서 같은 목록을 읽는다.
pub(super) fn observe_body(body: &str) -> Result<Option<Observed>, Error> {
    // 엄격한 JSON(중복 키 거절)으로만 읽는다.
    let Some(value) = parse(body) else {
        return Err(Error::Invalid);
    };
    let Some(blocks) = value.get("content").and_then(Value::as_array) else {
        return Ok(None);
    };
    let mut uses = Vec::new();
    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let (Some(id), Some(name), Some(input)) = (
            block.get("id").and_then(Value::as_str),
            block.get("name").and_then(Value::as_str),
            block.get("input").filter(|v| v.is_object()),
        ) else {
            return Err(Error::Invalid);
        };
        uses.push(ObservedUse {
            id: id.to_owned(),
            name: name.to_owned(),
            input: input.to_string(),
        });
    }
    let message = value.get("id").and_then(Value::as_str).map(str::to_owned);
    observed(message, uses)
}
