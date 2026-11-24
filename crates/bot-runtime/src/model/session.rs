//! Claude Code 세션 기록(JSONL)의 내구 복구 규약. 승인·위임으로 run이 멈추면 VM과 HOME은 사라지므로, 새 claim이 같은 대화를 이으려면
//! 세션의 **대화 줄**(`user`·`assistant`·`attachment`, `uuid` 체인)을 run별 체크포인트로 남겼다가 새 HOME의 알려진 파일 하나로 되살려야 한다.
//! 토큰·MCP 설정·인증·경로·비용 기록은 복원하지 않는다. 형식은 실제 CLI(2.1.331)의 세션 파일에서 관찰한 것이다.
//!
//! 체크포인트 키(`bot_core::cli`의 접두사·타입을 쓴다. 한 건 ≤ 900 KiB, 쓰기는 `RunStore::checkpoint_batch`):
//! - `cli-rec-<seq>`: 세션 줄 하나(원문 문자열). `seq`는 run 전체에서 이어지는 0부터의 번호이고 claim이 바뀌어도 이어진다.
//! - `cli-use-<tool_use_id>`: [`CliUse`] — 그 `tool_use`의 위치(`turn`·`pos`)와 CLI가 본 이름·원 입력.
//! - `cli-group-<turn>`: [`CliGroup`] — 한 모델 응답이 부른 모든 `tool_use` ID(순서대로). `turn`은 그 응답의 첫 assistant 줄 번호다.
//! - `cli-resp-<message.id>`: [`CliResponse`] — 브로커 프런트(③)가 제공자의 **완료한** 응답에서 읽어 쓴 도구 목록(읽기만 한다).
//!
//! 한 응답의 줄들·`cli-use`·`cli-group`은 **한 번의 `checkpoint_batch`**로만 쓴다(전부 아니면 하나도 없다). 프런트가 `cli-resp`로 확인한
//! 목록(ID·이름·입력·순서)과 세션 줄에서 모은 목록이 정확히 같을 때만 쓰고, 그전에는 어떤 도구도 실행되지 않는다(프런트가 그룹을 기다린다).
//! 호출 ID는 `CallId::cli(turn, pos)`다. 진짜 결과만 남긴다: 원장에서 끝나지 않은 호출의 자리표시 결과 줄이 보이면 그 뒤로는 쓰지 않고(얼림),
//! 재개 때 아직 결과 없는 `tool_use`는 호스트가 실제 결과로 정확히 한 번 채운다.

mod journal;
mod open;
mod restore;
mod tail;

pub use bot_core::cli::ToolOutput;
pub use journal::{Fed, Journal, Plan, Refused, Why};
pub use restore::{Corrupt, Saved, restore, result_line};
pub use tail::Tail;

use bot_core::{
    cli::{CliResponse, RECORD_PREFIX},
    command::{JsonPayload, Step},
    id::{ProfileId, RunId},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fmt, future::Future, pin::Pin, sync::Arc};

/// 세션 포트의 실패. `Lost`는 이 claim이 더는 소유자가 아니다(물러난다), `Unavailable`은 저장소·브로커 사정(다시 하면 된다)이다.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionError {
    Lost,
    Unavailable,
    Permanent(&'static str),
}

pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SessionError>> + Send + 'a>>;

/// 복원한 마지막 그룹에서 아직 결과가 없는 `tool_use` 한 자리.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pending {
    pub turn: u64,
    pub pos: u32,
    pub tool_use: ToolUse,
}

/// 호스트가 한 자리를 마무리한 결말.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Slot {
    /// 실제 결과(또는 거절·실패의 고정 단어와 `is_error`)
    Result(ToolOutput),
    /// 승인·보류·자식을 다시 기다려야 한다: 저장소가 run을 멈췄다(CLI를 띄우지 않는다)
    Parked,
}

/// 세션 복구에 호스트(워커)가 대 주는 것: 체크포인트 읽기·쓰기, 프런트가 확인한 응답, 호출 결과의 진짜 여부, 자리 마무리.
pub trait SessionPort: Send + Sync {
    /// claim 때 읽은 `work.steps`(복원용)
    fn steps(&self) -> &[Step];
    /// 체크포인트 묶음을 현재 claim으로 한 커밋에 쓴다.
    fn commit(&self, steps: Vec<Step>) -> PortFuture<'_, ()>;
    /// 프런트가 쓴 `cli-resp-<message.id>`. 아직 없으면 `None`.
    fn response(&self, message_id: &str) -> PortFuture<'_, Option<CliResponse>>;
    /// 이미 기록된 자리를 원래 계획(도구 이름·입력)으로 **새 실행 없이** 다시 읽는다: 끝난 호출이면 현재 권한·도구·인자로 다시 확인한 뒤의 결과
    /// (거절이면 고정 단어), 끝나지 않았으면 `None`이다. 세션 파일에 적힌 결과 본문은 믿지 않는다 — 쓸 때는 끝났는지만 보고,
    /// 되살릴 때는 이 값으로 덮어쓴다.
    fn recall<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, Option<ToolOutput>>;
    /// 이 자리의 호출이 끝났는가: 원장에 완료나 확정 거절이 있거나 실행할 것이 없는 호출이다. 재읽기·실행 없이 기록만 본다.
    /// 끝나지 않았다면 모델이 받은 결과는 실행의 결과가 아니다(실패 응답).
    fn ended<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, bool>;
    /// 결과 없는 자리를 현재 권한·승인으로 마무리한다(원장·브로커·자식 결과는 호스트가 안다).
    fn settle<'a>(&'a self, slot: &'a Pending) -> PortFuture<'a, Slot>;
}

/// `ModelRequest`가 실을 수 있게 `Debug`를 가진 핸들.
#[derive(Clone)]
pub struct Session(pub Arc<dyn SessionPort>);

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Session")
    }
}

/// 체크포인트 한 건의 상한(코어 1 MiB)에서 여유를 둔 줄 크기 상한. 넘는 줄은 쓰지 않는다(얼린다).
pub const MAX_LINE_BYTES: usize = 900 * 1024;

pub fn rec_key(seq: u64) -> String {
    format!("{RECORD_PREFIX}{seq}")
}

/// 줄 하나를 체크포인트 한 건으로.
pub fn rec_step(seq: u64, line: &str) -> Step {
    Step {
        key: rec_key(seq),
        body: JsonPayload(Value::String(line.to_owned()).to_string()),
    }
}

/// 다른 입력에서 같은 UUID 모양 문자열을 만든다(버전 5 모양). 같은 입력은 언제나 같은 값이다.
fn uuid_from(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    b[6] = (b[6] & 0x0f) | 0x50;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// 이 run의 세션 ID. 결정적이라 어느 claim·어느 호스트에서도 같다.
pub fn session_id(profile: &ProfileId, run: &RunId) -> String {
    uuid_from(&format!("bot-cli-session|{}|{}", profile.0, run.0))
}

/// CLI가 작업 폴더에서 정하는 프로젝트 폴더 이름: 영숫자가 아닌 글자는 모두 `-`다(`/bot` → `-bot`).
pub fn project_dir(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// 세션 줄 안의 `tool_use` 하나.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    pub input: Value,
}

fn blocks(v: &Value) -> impl Iterator<Item = &Value> {
    v.get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

fn kind(v: &Value) -> &str {
    v.get("type").and_then(Value::as_str).unwrap_or_default()
}

/// 대화 체인의 줄인가(`user`·`assistant`·`attachment` + `uuid`). 비용·큐·요청 형태 같은 부기 줄은 아니다.
pub fn is_chain(v: &Value) -> bool {
    matches!(kind(v), "user" | "assistant" | "attachment")
        && v.get("uuid").and_then(Value::as_str).is_some()
}

pub fn uuid_of(v: &Value) -> Option<&str> {
    v.get("uuid").and_then(Value::as_str)
}

/// 어시스턴트 줄이 속한 제공자 응답의 `message.id`.
pub fn message_id(v: &Value) -> Option<&str> {
    (kind(v) == "assistant")
        .then(|| v.get("message")?.get("id")?.as_str())
        .flatten()
}

/// 어시스턴트 줄의 `tool_use` 블록들(위치 순).
pub fn tool_uses(v: &Value) -> Vec<ToolUse> {
    if kind(v) != "assistant" {
        return Vec::new();
    }
    blocks(v)
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|b| {
            Some(ToolUse {
                id: b.get("id")?.as_str()?.to_owned(),
                name: b.get("name")?.as_str()?.to_owned(),
                input: b.get("input").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// 사용자 줄의 `tool_result` 블록들이 가리키는 `tool_use` ID.
pub fn tool_results(v: &Value) -> Vec<String> {
    if kind(v) != "user" {
        return Vec::new();
    }
    blocks(v)
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(|b| b.get("tool_use_id")?.as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests;
