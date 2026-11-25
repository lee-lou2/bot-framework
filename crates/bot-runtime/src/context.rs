//! 맥락 조립(순수 함수). 프로필 판본의 지침·스킬·지식, 요청자에게 보이는 기억, 같은 대화의 이전 차례로 모델 입력(`Assembled`)을 만든다.
//! 같은 입력이면 항상 같은 출력이다(현재 시각도 입력으로 받는다).
//!
//! 규칙(docs/profile-spec.md):
//! - 지침은 부모에서 자식까지 이미 합쳐진 `Agent.instructions`를 그대로 쓴다.
//! - 지식은 입력 글과 단어가 겹치는 문서만 점수 순으로 **최대 8개·합계 32 KiB**, 각 문서에 출처(relative_path)를 붙인다.
//!   한도를 넘으면 점수가 낮은 것부터 버리고 문서 중간은 자르지 않는다.
//! - 기억은 호출자가 이미 요청자·대화 종류로 거른 `MemoryHit`만 받는다(가시성 판정은 저장소 몫이다). 이 모듈은 개수·크기만 막는다. 기억은 구성원이 쓴 글이라 지침이 아니다:
//!   시스템 지침에는 본문을 넣지 않고(고정 안내 한 문단뿐), 이번 질문 바로 앞의 사용자 영역에 출처를 붙인 «비신뢰 참고 자료»로 싣는다(`memory.rs`).
//! - 도구는 (요청 범위 ∩ 에이전트 상한 ∩ 정책 상한)이 도구의 스코프를 **모두** 덮을 때만 보인다. 안내일 뿐 경계가 아니다 — 최종 판정은 브로커가 호출마다 다시 한다.
//! - 위임 대상은 직계 하위 에이전트뿐이다(`delegate` 도구).

use crate::model::{
    Exchange, Limits, Message, ModelRequest, RelayChannel, Role, Session, ToolKind, ToolSpec,
};
use bot_core::{
    command::{JsonPayload, MemoryHit, Target, Turn, Work},
    id::{AgentId, Attempt, ToolId},
    profile::{Agent, Builtin, Document, Snapshot},
    state::{Risk, RunContext},
};
use chrono::{DateTime, Datelike, Utc};
use chrono_tz::Tz;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

mod memory;
pub use memory::REFERENCE_HEADER;

pub const MAX_KNOWLEDGE_DOCS: usize = 8;
pub const MAX_KNOWLEDGE_BYTES: usize = 32 * 1024;
/// 기억 항목 수·합계 상한(저장소가 많이 돌려줘도 모델 입력이 불어나지 않게)
pub const MAX_MEMORY_ITEMS: usize = 16;
pub const MAX_MEMORY_BYTES: usize = 8 * 1024;
/// 이전 대화 합계 상한(오래된 차례부터 버린다)
pub const MAX_HISTORY_BYTES: usize = 64 * 1024;
/// 위임 도구의 ID. 워커가 이 ID를 가로채 `DelegationStore::delegate`로 보낸다.
pub const DELEGATE_TOOL: &str = "delegate";

const WEEKDAYS: [&str; 7] = ["월", "화", "수", "목", "금", "토", "일"];

pub struct ContextInput<'a> {
    pub snapshot: &'a Snapshot,
    pub run: &'a RunContext,
    /// 같은 대화의 이전 차례(오래된 순)
    pub history: &'a [Turn],
    /// 요청자에게 보이는 기억(저장소가 가시성으로 이미 거른 것)
    pub memory: &'a [MemoryHit],
    pub now: SystemTime,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextError {
    /// 판본에 이 run의 에이전트가 없다
    UnknownAgent(AgentId),
    /// 도구를 직접 부르는 run에는 모델 맥락이 없다
    NotAnAgentRun,
}

#[derive(Clone, Debug)]
pub struct Assembled {
    /// 모델 별칭(에이전트 → 부모 → 프로필 순으로 이미 풀린 값)
    pub model: String,
    pub system: String,
    pub conversation: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    /// 위임할 수 있는 직계 하위 에이전트
    pub delegates: Vec<AgentId>,
    /// 에이전트가 AGENT.md에 명시한 읽기 전용 내장 도구(상속 없음)
    pub builtin: BTreeSet<Builtin>,
    /// 고른 지식 문서의 출처(검증·관측용)
    pub knowledge: Vec<String>,
}

impl Assembled {
    /// 워커가 시도·교환·시간·취소·중계 끝점을 더해 실행기에 줄 요청을 만든다.
    pub fn into_request(
        self,
        attempt: Attempt,
        exchanges: Vec<Exchange>,
        limits: Limits,
        cancel: watch::Receiver<bool>,
        relay: Option<RelayChannel>,
        session: Option<Session>,
    ) -> ModelRequest {
        ModelRequest {
            attempt,
            model: self.model,
            system: self.system,
            conversation: self.conversation,
            tools: self.tools,
            exchanges,
            limits,
            cancel,
            builtin: self.builtin,
            relay,
            session,
        }
    }
}

// ── 지식 고르기 ───────────────────────────────────────────────────────────────────────────────────

/// 글자·숫자 덩어리(두 글자 이상)의 집합. 한글처럼 조사가 붙는 말은 아래 글자쌍으로 보완한다.
fn tokens(text: &str) -> BTreeSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2)
        .map(str::to_owned)
        .collect()
}

fn pairs(tokens: &BTreeSet<String>) -> BTreeSet<(char, char)> {
    let mut out = BTreeSet::new();
    for token in tokens {
        let chars: Vec<char> = token.chars().collect();
        out.extend(chars.windows(2).map(|w| (w[0], w[1])));
    }
    out
}

fn stem(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// 입력 글과 겹치는 정도: 같은 낱말 하나에 3점, 같은 글자쌍 하나에 1점. 문서의 글과 파일 이름을 본다.
fn score(query: &BTreeSet<String>, query_pairs: &BTreeSet<(char, char)>, doc: &Document) -> usize {
    let words = tokens(&format!("{} {}", stem(&doc.relative_path), doc.text));
    let shared_words = query.intersection(&words).count();
    let shared_pairs = query_pairs.intersection(&pairs(&words)).count();
    3 * shared_words + shared_pairs
}

/// 입력 글과 겹치는 문서를 점수 순(같으면 경로 순)으로 최대 8개·합계 32 KiB까지 고른다.
/// 한도를 넘는 문서는 통째로 건너뛰고 다음 문서를 본다(문서 중간을 자르지 않는다).
pub fn select_knowledge<'a>(query: &str, docs: &'a [Document]) -> Vec<&'a Document> {
    let words = tokens(query);
    if words.is_empty() {
        return vec![];
    }
    let query_pairs = pairs(&words);
    let mut scored: Vec<(usize, &Document)> = docs
        .iter()
        .filter(|d| !d.text.trim().is_empty())
        .filter_map(|d| Some((score(&words, &query_pairs, d), d)).filter(|(s, _)| *s > 0))
        .collect();
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.relative_path.cmp(&b.1.relative_path))
    });
    let (mut picked, mut used) = (Vec::new(), 0usize);
    for (_, doc) in scored {
        if picked.len() >= MAX_KNOWLEDGE_DOCS {
            break;
        }
        if used + doc.text.len() > MAX_KNOWLEDGE_BYTES {
            continue;
        }
        used += doc.text.len();
        picked.push(doc);
    }
    picked
}

// ── 입력 글 ───────────────────────────────────────────────────────────────────────────────────────

/// 본문 문서에서 사람이 읽을 글만: `{"text":…}`면 그 글, 비어 있는 문서(`{}`·null)는 빈 글, 그 밖은 JSON 그대로.
fn body_text(body: &JsonPayload) -> String {
    match serde_json::from_str::<Value>(&body.0) {
        Ok(Value::Null) => String::new(),
        Ok(Value::Object(map)) if map.is_empty() => String::new(),
        Ok(Value::Object(map)) => match map.get("text") {
            Some(Value::String(text)) => text.clone(),
            _ => Value::Object(map).to_string(),
        },
        Ok(Value::String(text)) => text,
        Ok(other) => other.to_string(),
        Err(_) => body.0.trim().to_owned(),
    }
}

/// 이번 입력: 트리거의 고정 지시와 본문을 합친다(같으면 한 번, 한쪽이 비면 다른 쪽만).
pub(crate) fn current_input(work: &Work) -> Result<String, ContextError> {
    let Target::Agent { instruction, .. } = &work.target else {
        return Err(ContextError::NotAnAgentRun);
    };
    let (instruction, body) = (instruction.trim(), body_text(&work.input));
    let body = body.trim();
    Ok(match (instruction.is_empty(), body.is_empty()) {
        (true, _) => body.to_owned(),
        (false, true) => instruction.to_owned(),
        (false, false) if instruction == body => body.to_owned(),
        (false, false) => format!("{instruction}\n\n입력:\n{body}"),
    })
}

// ── 문서 조각 ─────────────────────────────────────────────────────────────────────────────────────

fn clock(now: SystemTime, zone: &str) -> String {
    let tz: Tz = zone.parse().unwrap_or(chrono_tz::UTC);
    let secs = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0));
    let local = DateTime::<Utc>::from_timestamp(secs, 0)
        .unwrap_or_default()
        .with_timezone(&tz);
    let weekday = WEEKDAYS[local.weekday().num_days_from_monday() as usize];
    format!(
        "{}({weekday}) {} ({})",
        local.format("%Y-%m-%d"),
        local.format("%H:%M"),
        tz.name()
    )
}

/// 하위 에이전트를 소개하는 한 줄: 프로필이 선언한 `description`, 없으면 합쳐진 지침의 마지막 문단(그 에이전트 자신의 AGENT.md)의 앞부분
fn hint(agent: &Agent) -> String {
    let declared = agent.description.trim();
    if !declared.is_empty() {
        return declared
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(160)
            .collect::<String>()
            .trim()
            .to_owned();
    }
    let last = agent
        .instructions
        .rsplit("\n\n")
        .map(str::trim)
        .find(|p| !p.is_empty())
        .unwrap_or_default();
    let line: String = last
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(160)
        .collect();
    line.trim().to_owned()
}

fn description(schema: &JsonPayload, id: &ToolId) -> String {
    serde_json::from_str::<Value>(&schema.0)
        .ok()
        .and_then(|v| v.get("description")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("{} 도구", id.0))
}

/// 이 run이 쓸 수 있는 범위: 요청 범위 ∩ 에이전트 상한 ∩ 정책 상한
pub(crate) fn effective_scopes(
    snapshot: &Snapshot,
    agent: &Agent,
    run: &RunContext,
) -> BTreeSet<String> {
    run.scopes
        .iter()
        .filter(|s| agent.scopes.contains(*s) && snapshot.policy.scopes.contains(*s))
        .cloned()
        .collect()
}

fn visible_tools(agent: &Agent, effective: &BTreeSet<String>) -> Vec<(String, ToolSpec)> {
    let mut out = Vec::new();
    for (name, skill) in &agent.skills {
        for (id, tool) in &skill.tools {
            if tool.scopes.is_empty() || !tool.scopes.iter().all(|s| effective.contains(s)) {
                continue;
            }
            let spec = ToolSpec {
                id: id.clone(),
                kind: ToolKind::Skill,
                class: tool.class,
                description: description(&tool.input, id),
                input: tool.input.clone(),
            };
            out.push((name.clone(), spec));
        }
    }
    out
}

fn delegate_tool(children: &[(AgentId, String)]) -> ToolSpec {
    let ids: Vec<&str> = children.iter().map(|(id, _)| id.0.as_str()).collect();
    ToolSpec {
        id: ToolId(DELEGATE_TOOL.into()),
        kind: ToolKind::Delegate,
        class: Risk::Read,
        description: "직계 하위 에이전트에게 일을 맡기고 그 결과를 받아요.".into(),
        input: JsonPayload(
            json!({
                "type": "object",
                "required": ["agent", "instruction"],
                "properties": {
                    "agent": {"type": "string", "enum": ids},
                    "instruction": {"type": "string", "minLength": 1},
                },
                "additionalProperties": false,
            })
            .to_string(),
        ),
    }
}

fn history_messages(history: &[Turn]) -> Vec<Message> {
    let (mut kept, mut used) = (Vec::new(), 0usize);
    for turn in history.iter().rev() {
        if used + turn.text.len() > MAX_HISTORY_BYTES {
            break;
        }
        used += turn.text.len();
        kept.push(Message {
            role: if turn.from.is_some() {
                Role::User
            } else {
                Role::Assistant
            },
            speaker: turn.from.clone(),
            text: turn.text.clone(),
        });
    }
    kept.reverse();
    kept
}

/// 모델 입력을 조립한다.
pub fn assemble(input: &ContextInput<'_>) -> Result<Assembled, ContextError> {
    let (snapshot, run) = (input.snapshot, input.run);
    let agent = snapshot
        .agents
        .get(&run.agent)
        .ok_or_else(|| ContextError::UnknownAgent(run.agent.clone()))?;
    let question = current_input(&run.work)?;

    let effective = effective_scopes(snapshot, agent, run);
    let visible = visible_tools(agent, &effective);
    let mut tools: Vec<ToolSpec> = visible.iter().map(|(_, spec)| spec.clone()).collect();
    let children: Vec<(AgentId, String)> = snapshot
        .agents
        .iter()
        .filter(|(_, child)| child.parent.as_ref() == Some(&run.agent))
        .map(|(id, child)| (id.clone(), hint(child)))
        .collect();
    if !children.is_empty() {
        tools.push(delegate_tool(&children));
    }

    let query = match &run.work.target {
        Target::Agent { instruction, .. } => format!("{instruction}\n{question}"),
        Target::Tool { .. } => question.clone(),
    };
    let docs = select_knowledge(&query, &agent.knowledge);

    let mut sections = Vec::new();
    if !agent.instructions.trim().is_empty() {
        sections.push(agent.instructions.trim().to_owned());
    }
    sections.push(format!(
        "## 현재 시각\n{}",
        clock(input.now, &snapshot.timezone)
    ));
    let skills: Vec<String> = agent
        .skills
        .iter()
        .filter(|(name, skill)| {
            skill.tools.is_empty() || visible.iter().any(|(owner, _)| owner == *name)
        })
        .map(|(name, skill)| format!("### {name}\n{}", skill.instructions.text.trim()))
        .collect();
    if !skills.is_empty() {
        sections.push(format!("## 스킬\n{}", skills.join("\n\n")));
    }
    if !children.is_empty() {
        let lines: Vec<String> = children
            .iter()
            .map(|(id, hint)| {
                if hint.is_empty() {
                    format!("- {}", id.0)
                } else {
                    format!("- {}: {hint}", id.0)
                }
            })
            .collect();
        sections.push(format!(
            "## 위임\n직계 하위 에이전트에게만 `{DELEGATE_TOOL}` 도구로 일을 맡길 수 있어요.\n{}",
            lines.join("\n")
        ));
    }
    if !docs.is_empty() {
        let body: Vec<String> = docs
            .iter()
            .map(|d| format!("### {}\n{}", d.relative_path, d.text.trim_end()))
            .collect();
        sections.push(format!("## 참고 문서\n{}", body.join("\n\n")));
    }
    // 회상한 기억은 지침이 아니다: 시스템에는 본문 없이 고정 안내만 두고, 본문은 아래 대화의 이번 질문 앞(사용자 영역)에 «비신뢰 참고 자료»로 싣는다
    let reference = memory::reference(input.memory);
    if reference.is_some() {
        sections.push(memory::SYSTEM_NOTICE.to_owned());
    }

    let mut conversation = history_messages(input.history);
    conversation.push(Message {
        role: Role::User,
        speaker: Some(run.principal.clone()),
        text: question,
    });
    memory::place(&mut conversation, reference);
    Ok(Assembled {
        model: agent.model.clone(),
        system: sections.join("\n\n"),
        conversation,
        tools,
        delegates: children.into_iter().map(|(id, _)| id).collect(),
        builtin: agent.builtin.clone(),
        knowledge: docs.iter().map(|d| d.relative_path.clone()).collect(),
    })
}

#[cfg(test)]
mod tests;
