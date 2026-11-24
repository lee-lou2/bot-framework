//! 쓰는 쪽 상태. 세션 줄을 순서대로 먹고, 프런트(③)가 제공자의 완료한 응답에서 확인한 도구 목록과 정확히 맞을 때에만
//! 한 묶음(그때까지의 줄 + `cli-use` + `cli-group`)을 낸다. 순수한 상태 기계이고 입출력은 [`Tail`](super::Tail)이 맡는다.

use super::{
    MAX_LINE_BYTES, Pending, ToolUse, is_chain, message_id, rec_step, tool_results, tool_uses,
};
use bot_core::{
    cli::{CliGroup, CliResponse, CliUse, GROUP_PREFIX, USE_PREFIX},
    command::{JsonPayload, Step},
};
use serde_json::Value;
use std::collections::BTreeMap;

/// 줄 하나를 먹은 결과.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fed {
    /// 대화 체인의 줄이 아니거나 깨진 줄(건너뛴다)
    Skipped,
    Accepted,
    /// 상한을 넘는 줄: 이 claim은 더 쓰지 않는다
    Frozen,
}

/// 이 세션을 더 쓰지 않는 이유(얼린 사유).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Why {
    /// 끝나지 않은 호출의 자리표시 결과 등: 재개 때 호스트가 실제 결과로 채우면 이어진다
    Unsaved,
    /// 줄 하나나 쌓인 줄이 상한을 넘었다: 다시 해도 같다
    TooLarge,
}

/// 묶음을 낼 수 없는 이유. `NotYet`·`Incomplete`는 기다리면 되고 나머지는 이 세션을 믿을 수 없다는 뜻이다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Refused {
    /// 더 오래된 응답이 아직 커밋되지 않았다
    NotYet,
    /// 세션 줄에서 아직 다 모으지 못했다(프런트가 확인한 응답의 일부만 도착)
    Incomplete,
    /// 프런트가 확인한 목록과 세션 줄이 다르다(ID·이름·입력·순서·개수)
    Mismatch,
    /// 줄 안의 도구 결과가 어느 자리의 것인지 알 수 없다
    Unverified,
}

struct Line {
    seq: u64,
    raw: String,
    value: Value,
}

struct Located {
    seq: u64,
    tool: ToolUse,
}

/// 도구를 부른 제공자 응답 하나(줄들이 도착하는 대로 모은다).
struct Open {
    id: String,
    /// 이 응답의 첫 assistant 줄 번호
    turn: u64,
    uses: Vec<Located>,
}

/// 이미 자리가 정해진 `tool_use`(복원했거나 커밋한 것). 입력까지 가져 결과를 다시 확인할 때 원래 계획을 그대로 넘긴다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Known {
    pub turn: u64,
    pub pos: u32,
    pub name: String,
    pub input: Value,
}

/// 프런트 확인을 기다리는 줄의 합계 상한(바이트·줄 수). 넘으면 더 쓰지 않는다(`Frozen`).
const MAX_BUFFERED_BYTES: usize = 16 << 20;
const MAX_BUFFERED_LINES: usize = 4096;

/// 한 응답의 묶음. 쓰기 전에 `verify`의 자리들이 모두 끝난 호출인지 확인하고, 쓴 뒤에 [`Journal::applied`]로 알린다.
#[derive(Debug)]
pub struct Plan {
    /// rec들 → use들 → group(마지막)
    pub steps: Vec<Step>,
    /// 묶음에 든 도구 결과 줄이 가리키는 릴레이 도구(원래 계획 그대로). 끝난 호출이어야 한다.
    pub verify: Vec<Pending>,
    through: u64,
}

pub struct Journal {
    next: u64,
    buffer: Vec<Line>,
    open: Vec<Open>,
    /// 버퍼에 쌓인 줄의 바이트 합계
    buffered: usize,
    known: BTreeMap<String, Known>,
    /// `mcp__<서버>__`: 이 접두사의 도구만 원장이 결과를 안다(그 밖은 VM 내장이라 줄에 있는 결과를 그대로 둔다)
    relay_prefix: String,
    frozen: Option<Why>,
    /// 얼린 뒤에 저장하지 못한 릴레이 도구 호출이 있다. 프런트는 그룹이 저장되기를 기다리다 그 호출을 실패시킨다
    blocked: bool,
    /// 이 claim이 묶음으로 쓴 릴레이 도구 자리. 끝날 때 원장에서 모두 끝났는지 대조한다(복원한 자리는 복원 때 이미 마무리했다)
    saved: Vec<Pending>,
}

impl Journal {
    /// `next`는 이미 저장된 줄 수(다음 줄 번호)다.
    pub fn new(next: u64, known: BTreeMap<String, Known>, relay_prefix: String) -> Self {
        Self {
            next,
            buffer: Vec::new(),
            open: Vec::new(),
            buffered: 0,
            known,
            relay_prefix,
            frozen: None,
            blocked: false,
            saved: Vec::new(),
        }
    }

    pub fn frozen(&self) -> bool {
        self.frozen.is_some()
    }

    /// 얼린 채로 릴레이 도구 호출이 막혔다면 그 사유. 이 run은 성공으로 끝나면 안 된다.
    pub fn stuck(&self) -> Option<Why> {
        self.frozen.filter(|_| self.blocked)
    }

    /// 이 claim이 저장한 릴레이 도구 자리들(원래 계획 그대로).
    pub fn saved_relay(&self) -> &[Pending] {
        &self.saved
    }

    /// 확인을 기다리는 응답이 릴레이 도구를 불렀다: 아직 저장하지 못해 프런트가 실행하지 못한 호출이다.
    pub fn unsaved_relay(&self) -> bool {
        self.open
            .iter()
            .flat_map(|o| &o.uses)
            .any(|u| u.tool.name.starts_with(&self.relay_prefix))
    }

    /// 더 쓰지 않는다(자리표시 결과 등 이 세션을 이을 수 없다). 저장을 기다리던 릴레이 호출은 이제 실행되지 못한다.
    pub fn freeze(&mut self, why: Why) {
        self.blocked |= self.unsaved_relay();
        self.frozen.get_or_insert(why);
        self.buffer.clear();
        self.open.clear();
        self.buffered = 0;
    }

    /// 프런트의 확인을 기다리는 가장 오래된 응답의 `message.id`.
    pub fn oldest(&self) -> Option<&str> {
        self.open.first().map(|o| o.id.as_str())
    }

    /// 세션 줄 하나를 먹는다.
    pub fn feed(&mut self, raw: &str) -> Fed {
        if self.frozen.is_some() {
            // 얼린 뒤에도 릴레이 도구를 부르는 줄이 오면 그 호출은 저장되지 못한다(프런트가 그룹을 기다리다 실패시킨다)
            if !self.blocked && raw.contains("\"tool_use\"") {
                self.blocked = serde_json::from_str::<Value>(raw).is_ok_and(|v| {
                    tool_uses(&v)
                        .iter()
                        .any(|u| u.name.starts_with(&self.relay_prefix))
                });
            }
            return Fed::Skipped;
        }
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            return Fed::Skipped;
        };
        if !is_chain(&value) {
            return Fed::Skipped;
        }
        // 한 줄이 체크포인트 한 건에 안 들어가거나 확인 전에 쌓인 줄이 너무 많으면 더 쓰지 않는다
        if raw.len() > MAX_LINE_BYTES
            || self.buffered + raw.len() > MAX_BUFFERED_BYTES
            || self.buffer.len() >= MAX_BUFFERED_LINES
        {
            self.freeze(Why::TooLarge);
            self.blocked |= tool_uses(&value)
                .iter()
                .any(|u| u.name.starts_with(&self.relay_prefix));
            return Fed::Frozen;
        }
        self.buffered += raw.len();
        let seq = self.next;
        self.next += 1;
        let uses = tool_uses(&value);
        if !uses.is_empty()
            && let Some(id) = message_id(&value)
        {
            let at = match self.open.iter().position(|o| o.id == id) {
                Some(at) => at,
                None => {
                    // 응답의 첫 줄이 글만 있었을 수 있다: 같은 응답의 앞선 줄 번호를 응답의 시작으로 삼는다
                    let turn = self.first_line_of(id).unwrap_or(seq);
                    self.open.push(Open {
                        id: id.to_owned(),
                        turn,
                        uses: Vec::new(),
                    });
                    self.open.len() - 1
                }
            };
            self.open[at]
                .uses
                .extend(uses.into_iter().map(|tool| Located { seq, tool }));
        }
        self.buffer.push(Line {
            seq,
            raw: raw.to_owned(),
            value,
        });
        Fed::Accepted
    }

    /// 버퍼에서 이 `message.id`의 첫 assistant 줄 번호.
    fn first_line_of(&self, id: &str) -> Option<u64> {
        self.buffer
            .iter()
            .find(|l| message_id(&l.value) == Some(id))
            .map(|l| l.seq)
    }

    /// 프런트가 확인한 응답과 모은 줄이 정확히 같으면 묶음을 만든다. 가장 오래된 응답만 낸다(체인 순서를 지킨다).
    pub fn plan(&self, response: &CliResponse) -> Result<Plan, Refused> {
        let Some(open) = self.open.first() else {
            return Err(Refused::Mismatch);
        };
        if open.id != response.id {
            return Err(if self.open.iter().any(|o| o.id == response.id) {
                Refused::NotYet
            } else {
                Refused::Mismatch
            });
        }
        if !response.is_valid() || open.uses.len() > response.uses.len() {
            return Err(Refused::Mismatch);
        }
        for (got, want) in open.uses.iter().zip(&response.uses) {
            if got.tool.id != want.id || got.tool.name != want.name || got.tool.input != want.input
            {
                return Err(Refused::Mismatch);
            }
        }
        if open.uses.len() < response.uses.len() {
            return Err(Refused::Incomplete);
        }
        // 다음 응답의 첫 줄 앞까지가 이 응답의 묶음이다(앞선 응답의 도구 결과 줄과 이 응답의 줄)
        let end = self.open.get(1).map_or(u64::MAX, |next| next.turn);
        let included: Vec<&Line> = self.buffer.iter().filter(|l| l.seq < end).collect();
        let mut steps: Vec<Step> = included.iter().map(|l| rec_step(l.seq, &l.raw)).collect();
        let mut verify = Vec::new();
        for line in &included {
            for id in tool_results(&line.value) {
                let place = self.known.get(&id).map(|k| Pending {
                    turn: k.turn,
                    pos: k.pos,
                    tool_use: ToolUse {
                        id: id.clone(),
                        name: k.name.clone(),
                        input: k.input.clone(),
                    },
                });
                let place = place.or_else(|| {
                    let pos = open.uses.iter().position(|u| u.tool.id == id)?;
                    Some(Pending {
                        turn: open.turn,
                        pos: pos as u32,
                        tool_use: open.uses[pos].tool.clone(),
                    })
                });
                let Some(place) = place else {
                    return Err(Refused::Unverified);
                };
                if place.tool_use.name.starts_with(&self.relay_prefix) {
                    verify.push(place);
                }
            }
        }
        for (pos, u) in open.uses.iter().enumerate() {
            steps.push(step(
                format!("{USE_PREFIX}{}", u.tool.id),
                serde_json::to_value(CliUse {
                    seq: u.seq,
                    turn: open.turn,
                    pos: pos as u32,
                    name: u.tool.name.clone(),
                    input: u.tool.input.clone(),
                })
                .unwrap_or_default(),
            ));
        }
        steps.push(step(
            format!("{GROUP_PREFIX}{}", open.turn),
            serde_json::to_value(CliGroup {
                seq: open.turn,
                uses: open.uses.iter().map(|u| u.tool.id.clone()).collect(),
            })
            .unwrap_or_default(),
        ));
        Ok(Plan {
            steps,
            verify,
            through: end,
        })
    }

    /// 묶음을 썼다: 그 줄들을 버퍼에서 빼고 도구 자리를 기억한다.
    pub fn applied(&mut self, plan: &Plan) {
        if self.open.is_empty() {
            return;
        }
        let done = self.open.remove(0);
        for (pos, u) in done.uses.iter().enumerate() {
            if u.tool.name.starts_with(&self.relay_prefix) {
                self.saved.push(Pending {
                    turn: done.turn,
                    pos: pos as u32,
                    tool_use: u.tool.clone(),
                });
            }
            self.known.insert(
                u.tool.id.clone(),
                Known {
                    turn: done.turn,
                    pos: pos as u32,
                    name: u.tool.name.clone(),
                    input: u.tool.input.clone(),
                },
            );
        }
        self.buffer.retain(|l| l.seq >= plan.through);
        self.buffered = self.buffer.iter().map(|l| l.raw.len()).sum();
    }
}

fn step(key: String, body: Value) -> Step {
    Step {
        key,
        body: JsonPayload(body.to_string()),
    }
}
