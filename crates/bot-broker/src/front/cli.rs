//! CLI 재개 규약의 ③ 쪽 읽기. 같은 run의 체크포인트에 ②가 남긴 모델 차례의 계획과 `tools/call`을 대조해 논리 호출 ID
//! (`CallId::cli(turn, pos)`)를 정한다. 대조하지 못하면 아무것도 실행하지 않는다.
//!
//! 키와 타입은 `bot_core::cli`가 정한다(② 소유 기록):
//! - `cli-use-<tool_use_id>` = `CliUse {seq, turn, pos, name, input}` — 그 차례의 도구 한 건(`name`은 제공자가 본 이름
//!   `mcp__<릴레이>__<도구>`, `input`은 원 JSON).
//! - `cli-group-<turn>` = `CliGroup {seq: turn, uses: [원래 tool_use_id…]}` — 응답의 `rec`·`use`·`group`은 한 커밋으로 쓰이므로
//!   `use`가 보이면 그룹도 있다(응답이 아직 다 안 적혔을 때 첫 줄만 보고 실행하지 않는다).
//!
//! 한 도구 호출이 도착하면 `cli-use-<id>` 한 건을 제한 시간 동안 읽으며 기다리고(`CallLedger::step`), 보이면 그 차례의
//! `cli-group-<turn>`을 읽어 **그룹 전체**를 대조한다: 그룹의 모든 사용에 `cli-use`가 있고 차례·위치가 맞으며 ID가 겹치지 않아야
//! 한다(하나라도 빠졌거나 겹치면 이 도구도 실행하지 않는다). 이어서 이름이 같고(릴레이 접두사 포함) `input`의 정규 JSON이
//! 인자의 정규 JSON과 같을 때만 호출 ID를 준다. 기다림과 원장 읽기는 같은 절대 기한 안에 있다.

use super::CallBook;
use crate::{canon::canonicalize, json::parse, service::preflight::Preflight};
use bot_core::{
    cli::{CliGroup, CliRefusal, CliUse, GROUP_PREFIX, USE_PREFIX},
    command::{JsonPayload, ToolCall},
    id::{Attempt, CallId, ToolId},
    ledger::CallLedger,
    port::{Error, Reply},
};
use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::{Instant, sleep, timeout_at};

/// (프로필, run, 차례).
type Group = (String, String, u64);

/// 기본 기억 개수. 항목 하나는 글 두 개와 숫자 하나라 이만큼이면 수백 KB를 넘지 않는다.
const DEFAULT_CACHE: usize = 1024;

/// 전체를 대조해 본 그룹의 기억. 그룹 기록은 한 번 커밋되면 바뀌지 않으므로 잊어도 안전하다(다시 읽어 대조할 뿐이다).
/// 오래 떠 있는 프로세스에서 자라지 않게 크기에 상한을 두고, 넘으면 가장 오래된 것부터 잊는다.
struct Verified {
    limit: usize,
    order: VecDeque<Group>,
    seen: HashSet<Group>,
}

impl Verified {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            order: VecDeque::new(),
            seen: HashSet::new(),
        }
    }

    fn contains(&self, group: &Group) -> bool {
        self.seen.contains(group)
    }

    fn insert(&mut self, group: Group) {
        if self.limit == 0 {
            return; // 기억하지 않는 설정: 매번 다시 대조한다
        }
        if self.seen.insert(group.clone()) {
            self.order.push_back(group);
            while self.order.len() > self.limit {
                if let Some(oldest) = self.order.pop_front() {
                    self.seen.remove(&oldest);
                }
            }
        }
    }
}

/// 저장된 계획과 대조하는 운영 호출 원천.
pub struct PlannedCalls {
    ledger: Arc<dyn CallLedger>,
    wait: Duration,
    poll: Duration,
    /// CLI가 우리 MCP 서버에 붙이는 이름(제공자가 보는 도구 이름은 `mcp__<서버>__<도구>`다).
    server: String,
    /// 전체를 대조해 본 그룹. 상한이 있고 넘으면 오래된 것부터 잊는다.
    verified: Mutex<Verified>,
    /// 실행 전 확정 거절을 가리는 브로커의 사전 판정. 없으면 판정이 필요한 거절(`unknown_tool`·`invalid`·한도의 `unavailable`)은 표식을 만들지 않는다
    /// (재개가 한 번 다시 시도한다). 브로커의 거절 자체가 판정인 `denied`·`conflict`·`stale`은 사전 판정 없이도 표식을 남긴다.
    preflight: Option<Arc<dyn Preflight>>,
}

impl PlannedCalls {
    /// `wait`: 계획이 보일 때까지 기다리는 최대 시간(원장 읽기 시간을 포함한 절대 기한, 기본 15초 권장). 간격은 100ms다.
    pub fn new(ledger: Arc<dyn CallLedger>, wait: Duration) -> Self {
        Self {
            ledger,
            wait,
            poll: Duration::from_millis(100),
            server: "relay".to_owned(),
            verified: Mutex::new(Verified::new(DEFAULT_CACHE)),
            preflight: None,
        }
    }

    /// 브로커가 `Missing`·`Invalid`로 거절했거나 한도가 `Unavailable`로 돌려보낸 호출이 실행 전에 확정된 거절인지 가리는 사전 판정을 붙인다(운영은 `BrokerService`).
    /// 붙이면 확정 거절에 한해 프런트가 run 체크포인트에 표식을 남기고, 재개가 모델이 본 단어로 그 자리를 마무리한다.
    pub fn with_preflight(mut self, preflight: Arc<dyn Preflight>) -> Self {
        self.preflight = Some(preflight);
        self
    }

    /// 전체를 대조해 본 그룹을 기억하는 개수의 상한(기본 1024, 0이면 기억하지 않고 매번 대조한다).
    pub fn with_cache_limit(mut self, limit: usize) -> Self {
        self.verified = Mutex::new(Verified::new(limit));
        self
    }

    /// 지금 기억하고 있는 그룹 수(시험용).
    #[cfg(test)]
    pub(crate) fn cached_groups(&self) -> usize {
        self.verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .seen
            .len()
    }

    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// 기한 안에 키 한 건을 읽는다(기한을 넘기면 확인하지 못한 것이다).
    async fn step(
        &self,
        attempt: &Attempt,
        key: String,
        deadline: Instant,
    ) -> Result<Option<JsonPayload>, Error> {
        timeout_at(deadline, self.ledger.step(attempt.clone(), key))
            .await
            .map_err(|_| Error::Unavailable)?
    }

    /// 한 번 읽는다: 아직 안 보이면 `None`, 보이면 그룹 전체와 위치까지 맞춰 본 이 사용. 서로 어긋나면 `Invalid`.
    async fn read(
        &self,
        attempt: &Attempt,
        id: &str,
        deadline: Instant,
    ) -> Result<Option<CliUse>, Error> {
        let Some(body) = self
            .step(attempt, format!("{USE_PREFIX}{id}"), deadline)
            .await?
        else {
            return Ok(None);
        };
        let plan = use_of(&body)?;
        // 같은 커밋에 있어야 하는 그룹: 없거나 어긋나면 실행하지 않는다.
        let body = self
            .step(attempt, format!("{GROUP_PREFIX}{}", plan.turn), deadline)
            .await?
            .ok_or(Error::Invalid)?;
        let value = parse(&body.0).ok_or(Error::Invalid)?;
        let group: CliGroup = serde_json::from_value(value).map_err(|_| Error::Invalid)?;
        if group.seq != plan.turn
            || group.uses.get(plan.pos as usize).map(String::as_str) != Some(id)
            || plan.seq < plan.turn
        {
            return Err(Error::Invalid);
        }
        let key: Group = (attempt.profile.0.clone(), attempt.run.0.clone(), group.seq);
        if !self
            .verified
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key)
        {
            self.verify_group(attempt, &group, deadline).await?;
            self.verified
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key);
        }
        Ok(Some(plan))
    }

    /// 그룹의 모든 사용: ID가 겹치지 않고, 각각 `cli-use`가 있으며 차례·위치가 그룹과 같다.
    async fn verify_group(
        &self,
        attempt: &Attempt,
        group: &CliGroup,
        deadline: Instant,
    ) -> Result<(), Error> {
        let mut seen = HashSet::new();
        for (pos, id) in group.uses.iter().enumerate() {
            if !seen.insert(id.as_str()) {
                return Err(Error::Invalid);
            }
            let body = self
                .step(attempt, format!("{USE_PREFIX}{id}"), deadline)
                .await?
                .ok_or(Error::Invalid)?;
            let plan = use_of(&body)?;
            if plan.turn != group.seq || plan.pos as usize != pos || plan.seq < plan.turn {
                return Err(Error::Invalid);
            }
        }
        Ok(())
    }
}

fn use_of(body: &JsonPayload) -> Result<CliUse, Error> {
    let value = parse(&body.0).ok_or(Error::Invalid)?;
    serde_json::from_value(value).map_err(|_| Error::Invalid)
}

impl CallBook for PlannedCalls {
    fn call_id(
        &self,
        attempt: Attempt,
        tool_use: String,
        name: String,
        args: JsonPayload,
    ) -> Reply<'_, CallId> {
        Box::pin(async move {
            let deadline = Instant::now() + self.wait;
            loop {
                // 현재 claim을 쥔 시도만 읽는다(인계됐으면 Stale).
                if let Some(plan) = self.read(&attempt, &tool_use, deadline).await? {
                    // 계획과 도구 이름·정규 인자가 정확히 같아야 한다. 다르면 같은 id를 다른 호출에 쓰려는 것이다.
                    let same_args = canonicalize(&plan.input).as_deref() == Some(args.0.as_str());
                    // 제공자가 본 이름은 정확히 우리 릴레이 접두사가 붙은 이름이어야 한다(다른 서버나 내장 도구로는 대신할 수 없다).
                    let planned = format!("mcp__{}__{name}", self.server);
                    return if plan.name == planned && same_args {
                        Ok(CallId::cli(plan.turn, plan.pos))
                    } else {
                        Err(Error::Conflict)
                    };
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(Error::Unavailable); // 계획을 확인하지 못했다: 실행 0
                }
                sleep(self.poll.min(deadline - now)).await;
            }
        })
    }

    /// 이 위치에 이미 남은 확정 거절 표식. 같은 키를 다시 읽을 뿐이라 기한은 계획을 기다릴 때와 같고, 읽지 못하면 오류다(실행하지 않는다).
    /// 읽을 수 없는 표식은 없는 것으로 본다(런타임의 복원과 같다).
    fn marked(&self, attempt: Attempt, call: CallId) -> Reply<'_, Option<CliRefusal>> {
        Box::pin(async move {
            let deadline = Instant::now() + self.wait;
            let stored = self
                .step(&attempt, CliRefusal::key(&call), deadline)
                .await?;
            Ok(stored.and_then(|payload| serde_json::from_str::<CliRefusal>(&payload.0).ok()))
        })
    }

    /// 이 호출 ID(방금 저장된 계획과 대조해 정한 위치)에 대한 거절이 실행 전에 확정된 것인가. 순서는 ① 거절의 종류별 판정 ② 원장.
    /// ① `UnknownTool`·`Invalid`는 실행이 고정한 판본만 쓰는 사전 판정이 같은 답을 할 때만 확정이다. `Denied`·`Conflict`·`Stale`은 브로커의 거절 자체가
    /// 판정이다(`Stale`은 표식을 쓰는 현재 claim이 곧 증거다: claim을 잃었으면 쓰지 못한다). `RateLimited`는 부른 쪽이 **이 호출의 결과와 함께** 한도 거절을
    /// 확인했을 때만 묻는다(`front::refuse`가 `Unavailable`을 가린다) — 여기서는 다시 판정하지 않는다.
    /// ② 원장에 이 호출의 기록이 **없거나 아직 열려 있을(`Open`) 때만** 표식을 만든다. 시작·보류·완료·거절이 있으면 원장이 이미 이 호출을 말한다.
    /// 판정과 원장 읽기는 같은 절대 기한 안이다.
    fn confirm(
        &self,
        attempt: Attempt,
        call: CallId,
        tool: ToolId,
        args: JsonPayload,
        expected: CliRefusal,
    ) -> Reply<'_, bool> {
        Box::pin(async move {
            let call = ToolCall {
                attempt,
                call,
                tool,
                args,
            };
            tokio::time::timeout(
                self.wait,
                crate::service::preflight::confirm(
                    self.ledger.as_ref(),
                    self.preflight.as_deref(),
                    call,
                    expected,
                ),
            )
            .await
            .map_err(|_| Error::Unavailable)?
        })
    }
}
