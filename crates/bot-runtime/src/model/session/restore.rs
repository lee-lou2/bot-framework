//! 체크포인트에서 세션을 되살리고, 아직 결과 없는 `tool_use`를 호스트의 실제 결과로 채우는 결과 줄을 만든다(순수 함수).

use super::{
    Pending, ToolUse, journal::Known, message_id, tool_results, tool_uses, uuid_from, uuid_of,
};
use bot_core::{
    cli::{CliGroup, CliUse, GROUP_PREFIX, RECORD_PREFIX, ToolOutput, USE_PREFIX},
    command::Step,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// 기록이 모양을 어겼다(빠진 줄·깨진 JSON·계획 없는 그룹·줄과 다른 계획). 조용히 받아들이지 않고 재개하지 않는다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Corrupt;

/// 되살린 세션.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Saved {
    /// 저장된 모든 줄(원문, 번호순, 0부터 이어진다)
    pub lines: Vec<String>,
    /// 마지막 그룹이 담은 `tool_use`(위치 순)와 그 자리
    pub last_group: Vec<(u64, u32, ToolUse)>,
    /// 줄 안에 `tool_result`가 이미 있는 `tool_use` ID
    pub answered: BTreeSet<String>,
    /// 저장된 모든 그룹의 `tool_use` 자리
    pub known: BTreeMap<String, Known>,
    /// 마지막 줄의 uuid(결과 줄의 부모)와 그 줄에서 복사할 환경 값
    pub last_uuid: String,
    pub last: Value,
}

impl Saved {
    /// 아직 결과가 없는 `tool_use`(위치 순).
    pub fn pending(&self) -> Vec<Pending> {
        self.last_group
            .iter()
            .filter(|(_, _, u)| !self.answered.contains(&u.id))
            .map(|(turn, pos, tool_use)| Pending {
                turn: *turn,
                pos: *pos,
                tool_use: tool_use.clone(),
            })
            .collect()
    }

    /// 다음에 쓸 줄 번호.
    pub fn next_seq(&self) -> u64 {
        self.lines.len() as u64
    }

    /// 줄 안에 결과가 이미 있는 릴레이 도구(원래 계획 그대로). 그 결과 본문은 믿지 않고 현재 권한·도구·인자로 다시 확인한 값으로 덮어쓴다.
    pub fn relay_results(&self, prefix: &str) -> Vec<Pending> {
        self.answered
            .iter()
            .filter_map(|id| {
                let known = self.known.get(id)?;
                known.name.starts_with(prefix).then(|| Pending {
                    turn: known.turn,
                    pos: known.pos,
                    tool_use: ToolUse {
                        id: id.clone(),
                        name: known.name.clone(),
                        input: known.input.clone(),
                    },
                })
            })
            .collect()
    }

    /// 릴레이 도구의 결과 줄을 원장의 신뢰 결과로 바꾼다(`content`·`is_error`, CLI 부가 사본 `toolUseResult`는 지운다).
    /// 세션 파일은 VM이 쓴 것이라 본문을 그대로 믿지 않는다. 바꿀 결과가 없는 도구가 있으면 `Corrupt`다.
    pub fn trust(&mut self, trusted: &BTreeMap<String, ToolOutput>) -> Result<(), Corrupt> {
        for line in &mut self.lines {
            let mut value: Value = serde_json::from_str(line).map_err(|_| Corrupt)?;
            let mut changed = false;
            if let Some(Value::Array(blocks)) =
                value.get_mut("message").and_then(|m| m.get_mut("content"))
            {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                        continue;
                    };
                    if let Some(output) = trusted.get(id) {
                        block["content"] = Value::Array(output.content.clone());
                        block["is_error"] = Value::Bool(output.is_error);
                        changed = true;
                    }
                }
            }
            if changed {
                if let Some(record) = value.as_object_mut() {
                    record.remove("toolUseResult");
                }
                *line = value.to_string();
            }
        }
        self.last = serde_json::from_str(self.lines.last().ok_or(Corrupt)?).map_err(|_| Corrupt)?;
        Ok(())
    }
}

/// 체크포인트에서 세션을 되살린다. 그룹이 하나도 없으면 `None`(처음부터 시작한다).
pub fn restore(steps: &[Step]) -> Result<Option<Saved>, Corrupt> {
    let mut recs: BTreeMap<u64, String> = BTreeMap::new();
    let mut groups: BTreeMap<u64, CliGroup> = BTreeMap::new();
    let mut uses: BTreeMap<&str, CliUse> = BTreeMap::new();
    for step in steps {
        if let Some(seq) = step.key.strip_prefix(RECORD_PREFIX) {
            let line: Value = serde_json::from_str(&step.body.0).map_err(|_| Corrupt)?;
            recs.insert(
                seq.parse().map_err(|_| Corrupt)?,
                line.as_str().ok_or(Corrupt)?.to_owned(),
            );
        } else if let Some(turn) = step.key.strip_prefix(GROUP_PREFIX) {
            let turn: u64 = turn.parse().map_err(|_| Corrupt)?;
            let group: CliGroup = serde_json::from_str(&step.body.0).map_err(|_| Corrupt)?;
            if group.seq != turn {
                return Err(Corrupt);
            }
            groups.insert(turn, group);
        } else if let Some(id) = step.key.strip_prefix(USE_PREFIX) {
            uses.insert(
                id,
                serde_json::from_str::<CliUse>(&step.body.0).map_err(|_| Corrupt)?,
            );
        }
    }
    if groups.is_empty() {
        return Ok(None);
    }
    // 줄은 0부터 빠짐없이 이어진다(묶음 쓰기는 원자라 반쪽 줄이 없다)
    if recs.keys().copied().ne(0..recs.len() as u64) {
        return Err(Corrupt);
    }
    let lines: Vec<String> = recs.into_values().collect();
    let values: Vec<Value> = lines
        .iter()
        .map(|l| serde_json::from_str(l).map_err(|_| Corrupt))
        .collect::<Result<_, _>>()?;
    let mut answered = BTreeSet::new();
    for value in &values {
        answered.extend(tool_results(value));
    }

    let mut known = BTreeMap::new();
    let mut last_group = Vec::new();
    let newest = *groups.keys().next_back().ok_or(Corrupt)?;
    for (turn, group) in &groups {
        let mut slots = Vec::new();
        for (pos, id) in group.uses.iter().enumerate() {
            let plan = uses.get(id.as_str()).ok_or(Corrupt)?;
            // 계획은 줄에 있는 그대로여야 한다(`cli-use`는 프런트가 읽는 사본이다)
            let line = values.get(plan.seq as usize).ok_or(Corrupt)?;
            let in_line = tool_uses(line)
                .into_iter()
                .find(|u| u.id == *id)
                .ok_or(Corrupt)?;
            if plan.turn != *turn
                || plan.pos as usize != pos
                || in_line.name != plan.name
                || in_line.input != plan.input
                || *turn > plan.seq
                || values.get(*turn as usize).and_then(message_id) != message_id(line)
            {
                return Err(Corrupt);
            }
            known.insert(
                id.clone(),
                Known {
                    turn: *turn,
                    pos: pos as u32,
                    name: in_line.name.clone(),
                    input: in_line.input.clone(),
                },
            );
            slots.push((*turn, pos as u32, in_line));
        }
        if *turn == newest {
            last_group = slots;
        } else if slots.iter().any(|(_, _, u)| !answered.contains(&u.id)) {
            return Err(Corrupt); // 앞선 응답은 결과가 모두 있어야 다음 응답이 있다
        }
    }
    let last = values.last().cloned().ok_or(Corrupt)?;
    let last_uuid = uuid_of(&last).ok_or(Corrupt)?.to_owned();
    Ok(Some(Saved {
        lines,
        last_group,
        answered,
        known,
        last_uuid,
        last,
    }))
}

/// 결과 줄: 아직 결과 없는 `tool_use`를 한 줄(`tool_result` 블록 여럿)로 채운다. 같은 입력이면 같은 줄이다(uuid도 결정적).
/// 부모는 되살린 마지막 줄, 같은 세션·작업 폴더·버전을 이어 쓴다. 채울 것이 없으면 `None`이다.
pub fn result_line(
    saved: &Saved,
    session: &str,
    outcomes: &[(String, ToolOutput)],
    now: &str,
) -> Option<String> {
    if outcomes.is_empty() {
        return None;
    }
    let uuid = uuid_from(&format!(
        "bot-cli-result|{session}|{}",
        outcomes
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    ));
    let content: Vec<Value> = outcomes
        .iter()
        .map(|(id, out)| {
            json!({"type": "tool_result", "tool_use_id": id, "content": out.content, "is_error": out.is_error})
        })
        .collect();
    let copy = |key: &str| saved.last.get(key).cloned().unwrap_or(Value::Null);
    let record = json!({
        "parentUuid": saved.last_uuid,
        "isSidechain": false,
        "type": "user",
        "uuid": uuid,
        "sessionId": session,
        "cwd": copy("cwd"),
        "version": copy("version"),
        "userType": copy("userType"),
        "entrypoint": copy("entrypoint"),
        "gitBranch": copy("gitBranch"),
        "timestamp": now,
        "message": {"role": "user", "content": content},
        "sourceToolAssistantUUID": saved.last_uuid,
    });
    Some(record.to_string())
}
