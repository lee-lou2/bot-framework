//! 외부 호출 없는 결정적 데모 모델(`RunnerKind::Step`). README의 개인 비서 흐름(일정 조회·삭제)을 외부 계정 없이 확인하려고 쓴다.
//!
//! 상태는 `request.exchanges`(이번 run에서 이미 한 도구 호출)에서만 읽는다. 같은 입력이면 항상 같은 차례를 낸다.
//! - "일정 알려줘" 류: `agenda.list` 호출 → 결과를 한국어로 정리해 답한다.
//! - "…삭제해줘" 류: `agenda.list`로 후보를 찾고 제목이 말에 든 일정 하나에 `agenda.delete(event=id)` → 결과를 확인해 답한다.
//!   승인 대기·보류는 워커 몫이다(run이 멈춰 있는 동안 이 모델은 불리지 않고, 풀린 뒤 결과가 들어온 차례부터 다시 불린다).
//! - 그 밖의 말: 쓰는 법을 안내한다.

use super::{
    CallEnd, DoneCall, ModelError, ModelRequest, ModelRunner, ModelStep, PendingCall, Role,
    RunnerKind, StepFuture, Usage, call_id,
};
use bot_core::{command::JsonPayload, id::ToolId};
use serde_json::{Value, json};

const LIST: &str = "agenda.list";
const DELETE: &str = "agenda.delete";
const HELP: &str =
    "저는 데모 모델이에요. '오늘 일정 알려줘'나 '산책 일정을 삭제해줘'처럼 말해 보세요.";

#[derive(Clone, Copy, Debug, Default)]
pub struct DemoModel;

impl ModelRunner for DemoModel {
    fn kind(&self) -> RunnerKind {
        RunnerKind::Step
    }

    fn step(&self, request: ModelRequest) -> StepFuture<'_> {
        Box::pin(async move {
            if *request.cancel.borrow() {
                return Err(ModelError::Cancelled);
            }
            Ok(decide(&request))
        })
    }
}

fn say(text: impl Into<String>) -> ModelStep {
    ModelStep::Final {
        text: text.into(),
        usage: Usage::default(),
        outcome: Default::default(),
    }
}

/// 이번 차례에 부를 도구 하나. 호출 ID는 (run, 차례, 순번)에서 정해져 재개해도 같다.
fn call(request: &ModelRequest, tool: &str, args: Value, text: &str) -> ModelStep {
    ModelStep::Calls {
        text: text.into(),
        calls: vec![PendingCall {
            call: call_id(&request.attempt.run, request.exchanges.len(), 0),
            tool: ToolId(tool.into()),
            args: JsonPayload(args.to_string()),
        }],
        usage: Usage::default(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Intent {
    List,
    Delete,
    Other,
}

fn intent(text: &str) -> Intent {
    let t = text.to_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if any(&["삭제", "지워", "지우", "delete", "remove"]) {
        Intent::Delete
    } else if any(&["일정", "스케줄", "agenda", "calendar", "schedule"]) {
        Intent::List
    } else {
        Intent::Other
    }
}

fn decide(request: &ModelRequest) -> ModelStep {
    let text = request
        .conversation
        .iter()
        .rev()
        .find(|m| m.role == Role::User)
        .map(|m| m.text.clone())
        .unwrap_or_default();
    let has = |id: &str| request.tools.iter().any(|t| t.id.0 == id);
    match intent(&text) {
        Intent::Other => say(HELP),
        _ if !has(LIST) => {
            say("일정 도구를 쓸 수 없어요. agenda 스킬과 calendar.read 권한을 확인해 주세요.")
        }
        Intent::List => match request.exchanges.first() {
            None => call(request, LIST, json!({}), "일정을 확인해 볼게요."),
            Some(first) => say(render_list(first.calls.first())),
        },
        Intent::Delete => delete_flow(request, &text, has(DELETE)),
    }
}

fn delete_flow(request: &ModelRequest, text: &str, can_delete: bool) -> ModelStep {
    let exchanges = &request.exchanges;
    let Some(listed) = exchanges.first() else {
        return call(request, LIST, json!({}), "지울 일정을 먼저 찾아볼게요.");
    };
    let events = match parse(listed.calls.first()) {
        Parsed::Events(events) => events,
        Parsed::Text(_) | Parsed::Unavailable => {
            return say("일정을 확인하지 못해 삭제하지 않았어요.");
        }
    };
    let target = match pick(&events, text) {
        Pick::One(event) => event,
        Pick::None => {
            return say(format!(
                "말씀하신 일정을 찾지 못했어요.\n{}",
                render_events(&events)
            ));
        }
        Pick::Many(titles) => {
            return say(format!(
                "맞는 일정이 여러 개예요: {}. 어떤 일정을 삭제할까요?",
                titles.join(", ")
            ));
        }
    };
    match exchanges.get(1).and_then(|e| e.calls.first()) {
        None if !can_delete => {
            say("삭제 도구를 쓸 수 없어요. calendar.delete 권한을 확인해 주세요.")
        }
        None => call(
            request,
            DELETE,
            json!({"event": target.id}),
            &format!("'{}' 일정을 삭제할게요.", target.title),
        ),
        Some(DoneCall {
            end: CallEnd::Done(payload),
            ..
        }) => {
            // 시뮬레이션 서버의 결과는 영구 삭제처럼 말하지 않는다(`simulated: true`와 가상 영수증)
            let simulated = serde_json::from_str::<Value>(&payload.0)
                .ok()
                .and_then(|v| simulation(&v));
            match simulated {
                Some(Some(receipt)) => say(format!(
                    "'{}' 일정 삭제를 시뮬레이션했어요(가상 영수증 {receipt}).",
                    target.title
                )),
                Some(None) => say(format!(
                    "'{}' 일정 삭제를 시뮬레이션했어요(영구 삭제는 아니에요).",
                    target.title
                )),
                None => say(format!("'{}' 일정을 삭제했어요.", target.title)),
            }
        }
        Some(DoneCall {
            end: CallEnd::Denied,
            ..
        }) => say(format!(
            "삭제가 승인되지 않아 '{}' 일정은 그대로 두었어요.",
            target.title
        )),
        Some(DoneCall {
            end: CallEnd::Failed(_),
            ..
        }) => say("삭제하지 못했어요. 잠시 뒤 다시 시도해 주세요."),
    }
}

struct Event {
    id: String,
    title: String,
    when: Option<String>,
}

enum Parsed {
    Events(Vec<Event>),
    /// 일정 목록으로 읽을 수 없는 결과: 글만 있다
    Text(String),
    /// 호출이 실패했거나 막혔다
    Unavailable,
}

fn event_of(v: &Value) -> Option<Event> {
    let obj = v.as_object()?;
    let id = match obj.get("id")? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let text = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| obj.get(*k)?.as_str())
            .map(str::to_owned)
    };
    Some(Event {
        id,
        title: text(&["title", "name", "summary", "subject"])?,
        when: text(&["start", "at", "time", "when", "date"]),
    })
}

/// 흔한 결과 모양(배열 · `events` 등의 키 · MCP `structuredContent`/`content` 글 속 JSON)에서 일정 목록을 찾는다.
fn collect(v: &Value) -> Option<Vec<Event>> {
    match v {
        Value::Array(items) => {
            let events: Vec<Event> = items.iter().filter_map(event_of).collect();
            (items.is_empty() || !events.is_empty()).then_some(events)
        }
        Value::Object(map) => {
            if let Some(inner) = map.get("structuredContent") {
                return collect(inner);
            }
            for key in ["events", "items", "agenda", "results"] {
                if let Some(events) = map.get(key).and_then(collect) {
                    return Some(events);
                }
            }
            if let Some(Value::Array(blocks)) = map.get("content") {
                for block in blocks {
                    let inner = block
                        .get("text")
                        .and_then(Value::as_str)
                        .and_then(|t| serde_json::from_str::<Value>(t).ok());
                    if let Some(events) = inner.as_ref().and_then(collect) {
                        return Some(events);
                    }
                }
            }
            event_of(v).map(|e| vec![e])
        }
        _ => None,
    }
}

/// 삭제 결과가 시뮬레이션인가(`simulated: true`). 맞으면 가상 영수증(있으면, 짧게 자른 글)을 함께 준다.
/// 결과는 JSON 그대로이거나 MCP `structuredContent`·`content` 글 조각 속 JSON이다.
fn simulation(v: &Value) -> Option<Option<String>> {
    let map = v.as_object()?;
    if map.get("simulated").and_then(Value::as_bool) == Some(true) {
        return Some(
            map.get("receipt")
                .and_then(Value::as_str)
                .map(|r| r.chars().filter(|c| !c.is_control()).take(64).collect()),
        );
    }
    if let Some(found) = map.get("structuredContent").and_then(simulation) {
        return Some(found);
    }
    let Some(Value::Array(blocks)) = map.get("content") else {
        return None;
    };
    blocks.iter().find_map(|block| {
        let inner = serde_json::from_str::<Value>(block.get("text")?.as_str()?).ok()?;
        simulation(&inner)
    })
}

/// 결과를 사람이 읽을 글로: 문자열 · MCP `content`의 글 조각 · 그 밖은 JSON 그대로
fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(map) => match map.get("content") {
            Some(Value::Array(blocks)) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => v.to_string(),
        },
        _ => v.to_string(),
    }
}

fn parse(call: Option<&DoneCall>) -> Parsed {
    let Some(DoneCall {
        end: CallEnd::Done(payload),
        ..
    }) = call
    else {
        return Parsed::Unavailable;
    };
    match serde_json::from_str::<Value>(&payload.0) {
        Ok(value) => collect(&value).map_or_else(|| Parsed::Text(plain(&value)), Parsed::Events),
        Err(_) => Parsed::Text(payload.0.trim().to_owned()),
    }
}

fn render_events(events: &[Event]) -> String {
    if events.is_empty() {
        return "등록된 일정이 없어요.".into();
    }
    let lines: Vec<String> = events
        .iter()
        .map(|e| match &e.when {
            Some(when) => format!("- {when} {}", e.title),
            None => format!("- {}", e.title),
        })
        .collect();
    format!("일정이 {}건 있어요.\n{}", events.len(), lines.join("\n"))
}

fn render_list(call: Option<&DoneCall>) -> String {
    match parse(call) {
        Parsed::Events(events) => render_events(&events),
        Parsed::Text(text) if text.is_empty() => "등록된 일정이 없어요.".into(),
        Parsed::Text(text) => text,
        Parsed::Unavailable => match call.map(|c| &c.end) {
            Some(CallEnd::Denied) => "일정을 조회할 권한이 없어요.".into(),
            _ => "일정을 가져오지 못했어요. 잠시 뒤 다시 시도해 주세요.".into(),
        },
    }
}

enum Pick<'a> {
    One(&'a Event),
    None,
    /// 맞는 일정이 여럿이라 고를 수 없다(제목들)
    Many(Vec<String>),
}

/// 제목이 사용자의 말에 들어 있는 일정을 고른다. 여럿이면 다른 제목에 포함되지 않는(더 구체적인) 것만 남긴다.
fn pick<'a>(events: &'a [Event], text: &str) -> Pick<'a> {
    let said = text.to_lowercase();
    let hits: Vec<&Event> = events
        .iter()
        .filter(|e| !e.title.trim().is_empty() && said.contains(&e.title.trim().to_lowercase()))
        .collect();
    let specific: Vec<&Event> = hits
        .iter()
        .copied()
        .filter(|e| {
            !hits
                .iter()
                .any(|o| o.title.len() > e.title.len() && o.title.contains(&e.title))
        })
        .collect();
    match specific.as_slice() {
        [] => Pick::None,
        [one] => Pick::One(one),
        many => Pick::Many(many.iter().map(|e| e.title.clone()).collect()),
    }
}

#[cfg(test)]
mod tests;
