//! 시험용 메모리 포트: 브로커(결정 기록)·원장(승인 조회)·outbox(회신 예약·정산 기록).
use crate::active::{ActiveCheck, ActiveSet};
use bot_core::{
    cards::CardReceipt,
    command::{CallResult, JsonPayload, MemorySeed, ToolCall, TriggerSpec},
    id::*,
    intake::{DeliveryKey, DeliveryOutcome, DeliveryScope, Outbox, Outgoing, PushTransportId},
    ledger::{
        ApprovalState, CallLedger, CallRecord, Decision, FinishCall, OpenApproval, StartCall,
        StoredApproval,
    },
    port::{Broker, Error, Reply},
    profile::ActiveRevisions,
    state::{Approval, Risk, RunContext},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

fn unavailable<T: Send + 'static>() -> Reply<'static, T> {
    Box::pin(std::future::ready(Err(Error::Unavailable)))
}

/// 결정(`decide`) 호출을 기록하고, 정한 결과를 돌려주는 브로커
#[derive(Default)]
pub struct MemoryBroker {
    decisions: Mutex<Vec<(String, String, bool)>>,
    attempts: Mutex<usize>,
    next: Mutex<Option<Error>>,
}

impl MemoryBroker {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 성공한 결정만: (nonce, 주체, 허용 여부) 목록
    pub fn decisions(&self) -> Vec<(String, String, bool)> {
        self.decisions.lock().unwrap().clone()
    }

    /// 실패까지 포함한 `decide` 호출 횟수
    pub fn attempts(&self) -> usize {
        *self.attempts.lock().unwrap()
    }

    /// 다음 결정 한 번을 이 오류로 실패시킨다.
    pub fn fail_next(&self, error: Error) {
        *self.next.lock().unwrap() = Some(error);
    }
}

impl Broker for MemoryBroker {
    fn call(&self, _: ToolCall) -> Reply<'_, CallResult> {
        unavailable()
    }

    fn decide(&self, nonce: ApprovalId, actor: PrincipalId, allow: bool) -> Reply<'_, ()> {
        *self.attempts.lock().unwrap() += 1;
        let result = match self.next.lock().unwrap().take() {
            Some(error) => Err(error),
            None => {
                self.decisions
                    .lock()
                    .unwrap()
                    .push((nonce.0, actor.0, allow));
                Ok(())
            }
        };
        Box::pin(std::future::ready(result))
    }
}

/// 승인 조회만 하는 원장
#[derive(Default)]
pub struct MemoryLedger {
    approvals: Mutex<BTreeMap<String, (String, ApprovalState, String)>>,
    /// 소유자 DM으로 전달해 게시를 확인한 카드의 영수증(승인 읽기에 `delivery`로 실린다)
    deliveries: Mutex<BTreeMap<String, CardReceipt>>,
    fail_next: Mutex<Option<Error>>,
}

impl MemoryLedger {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 프로필 `profile`의 승인 카드 하나를 등록한다. `card`는 카드 JSON(nonce/title/lines).
    pub fn put(&self, nonce: &str, profile: &str, state: ApprovalState, card: &str) {
        self.approvals
            .lock()
            .unwrap()
            .insert(nonce.into(), (profile.into(), state, card.into()));
    }

    /// 이 승인의 카드를 소유자 DM으로 전달해 게시를 확인했다(저장소가 `Posted` 정산과 한 커밋에 하는 일).
    pub fn deliver(&self, nonce: &str, receipt: CardReceipt) {
        self.deliveries
            .lock()
            .unwrap()
            .insert(nonce.into(), receipt);
    }

    /// 다음 승인 조회 한 번을 이 오류로 실패시킨다.
    pub fn fail_next(&self, error: Error) {
        *self.fail_next.lock().unwrap() = Some(error);
    }

    pub fn set_state(&self, nonce: &str, state: ApprovalState) {
        if let Some(entry) = self.approvals.lock().unwrap().get_mut(nonce) {
            entry.1 = state;
        }
    }
}

impl CallLedger for MemoryLedger {
    fn approval_context(&self, _: ApprovalId, _: PrincipalId) -> Reply<'_, RunContext> {
        unavailable()
    }

    fn context(&self, _: Attempt) -> Reply<'_, RunContext> {
        unavailable()
    }

    fn lookup(&self, _: Attempt, _: CallId) -> Reply<'_, Option<CallRecord>> {
        unavailable()
    }

    fn active_revision(&self, _: Attempt) -> Reply<'_, Option<Revision>> {
        unavailable()
    }

    fn step(&self, _: Attempt, _: String) -> Reply<'_, Option<JsonPayload>> {
        unavailable()
    }

    fn begin(&self, _: Attempt, _: CallId, _: ToolId, _: [u8; 32]) -> Reply<'_, CallRecord> {
        unavailable()
    }

    fn open_approval(&self, _: OpenApproval) -> Reply<'_, ()> {
        unavailable()
    }

    fn approval(&self, nonce: ApprovalId) -> Reply<'_, StoredApproval> {
        if let Some(error) = self.fail_next.lock().unwrap().take() {
            return Box::pin(std::future::ready(Err(error)));
        }
        let found = self
            .approvals
            .lock()
            .unwrap()
            .get(&nonce.0)
            .map(|(profile, state, card)| StoredApproval {
                approval: Approval {
                    nonce: nonce.clone(),
                    attempt: Attempt {
                        profile: ProfileId(profile.clone()),
                        run: RunId("run-1".into()),
                        claim: Some(bot_core::id::ClaimEpoch(1)),
                        generation: Generation(1),
                    },
                    call: CallId("call-1".into()),
                    requester: PrincipalId("slack:home/U1".into()),
                    approver: PrincipalId("slack:home/U1".into()),
                    risk: Risk::Irreversible,
                    policy_revision: Revision("p1".into()),
                    tool: ToolId("agenda.delete".into()),
                    tool_revision: Revision("t1".into()),
                    args_sha256: [0; 32],
                    expires_at: SystemTime::now() + Duration::from_secs(600),
                },
                reasons: vec![],
                card: JsonPayload(card.clone()),
                state: *state,
                delivery: self.deliveries.lock().unwrap().get(&nonce.0).cloned(),
            });
        Box::pin(std::future::ready(found.ok_or(Error::Missing)))
    }

    fn supersede(&self, _: ApprovalId, _: PrincipalId) -> Reply<'_, ()> {
        unavailable()
    }

    fn decide(&self, _: Decision) -> Reply<'_, ApprovalState> {
        unavailable()
    }

    fn start(&self, _: StartCall) -> Reply<'_, ()> {
        unavailable()
    }

    fn finish(&self, _: FinishCall) -> Reply<'_, ()> {
        unavailable()
    }
}

/// 회신 예약을 스스로 잡고(claim) 정산 기록을 남기는 outbox
#[derive(Default)]
pub struct MemoryOutbox {
    inner: Mutex<OutboxInner>,
}

#[derive(Default)]
struct OutboxInner {
    pending: Vec<Outgoing>,
    leased: Vec<DeliveryKey>,
    begun: Vec<(DeliveryKey, String)>,
    settled: Vec<(DeliveryKey, String)>,
    fail_begin: bool,
}

fn describe(outcome: &DeliveryOutcome) -> String {
    match outcome {
        DeliveryOutcome::Confirmed { receipt } => format!("confirmed:{receipt}"),
        DeliveryOutcome::NotSent {
            reason,
            retry_after,
        } => {
            format!(
                "notsent:{reason:?}:{}",
                retry_after.map_or(0, |d| d.as_secs())
            )
        }
        DeliveryOutcome::Rejected => "rejected".into(),
        DeliveryOutcome::Held => "held".into(),
    }
}

impl MemoryOutbox {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 회신 예약 하나를 넣는다.
    pub fn push(&self, item: Outgoing) {
        self.inner.lock().unwrap().pending.push(item);
    }

    /// 이후 `begin`을 모두 거절한다(리스 상실·판본 불일치 흉내).
    pub fn fail_begin(&self, fail: bool) {
        self.inner.lock().unwrap().fail_begin = fail;
    }

    /// (키, 설명) — 정산 순서대로. 예: `confirmed:C1:1700000000.000001`, `notsent:RateLimited:9`, `rejected`, `held`.
    pub fn settled(&self) -> Vec<(DeliveryKey, String)> {
        self.inner.lock().unwrap().settled.clone()
    }

    /// `begin`에 넘어온 (키, 확인한 판본)
    pub fn begun(&self) -> Vec<(DeliveryKey, String)> {
        self.inner.lock().unwrap().begun.clone()
    }
}

impl Outbox for MemoryOutbox {
    fn claim<'a>(&'a self, scope: &'a DeliveryScope, limit: u32) -> Reply<'a, Vec<Outgoing>> {
        let mut inner = self.inner.lock().unwrap();
        let mut taken = Vec::new();
        let mut rest = Vec::new();
        for item in std::mem::take(&mut inner.pending) {
            if taken.len() < limit as usize && scope.contains(&item) {
                inner.leased.push(item.key.clone());
                taken.push(item);
            } else {
                rest.push(item);
            }
        }
        inner.pending = rest;
        Box::pin(std::future::ready(Ok(taken)))
    }

    fn begin(&self, key: DeliveryKey, checked_revision: Revision) -> Reply<'_, ()> {
        let mut inner = self.inner.lock().unwrap();
        let ok = !inner.fail_begin
            && inner.leased.contains(&key)
            && !inner.begun.iter().any(|(k, _)| *k == key);
        if ok {
            inner.begun.push((key, checked_revision.0));
        }
        Box::pin(std::future::ready(if ok {
            Ok(())
        } else {
            Err(Error::Stale)
        }))
    }

    fn settle(&self, key: DeliveryKey, outcome: DeliveryOutcome) -> Reply<'_, ()> {
        let mut inner = self.inner.lock().unwrap();
        inner.settled.push((key, describe(&outcome)));
        Box::pin(std::future::ready(Ok(())))
    }
}

/// 회신 예약 한 건 조립
pub fn outgoing(
    profile: &str,
    seq: u32,
    binding: &str,
    principal: &str,
    destination: bot_core::state::Conversation,
    body: &str,
) -> Outgoing {
    Outgoing {
        transport: PushTransportId::SLACK,
        key: DeliveryKey {
            profile: ProfileId(profile.into()),
            run: RunId("run-1".into()),
            seq,
            generation: Generation(1),
        },
        call: CallId(format!("reply-{seq}")),
        binding: BindingId(binding.into()),
        principal: PrincipalId(principal.into()),
        destination,
        body: JsonPayload(body.into()),
    }
}

/// 저장소가 게시한 활성 집합 판본을 흉내 낸다. 다른 프로세스가 새 판본을 활성화한 상황을 만든다.
pub struct MemoryActive {
    current: Mutex<Option<Revision>>,
    /// `current()`가 답하기 전에 기다리는 시간(저장소가 느린 상황)
    delay: Mutex<std::time::Duration>,
}

impl MemoryActive {
    pub fn new(current: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            current: Mutex::new(current.map(|r| Revision(r.to_owned()))),
            delay: Mutex::new(std::time::Duration::ZERO),
        })
    }

    /// 앞으로의 `current()`가 이 시간만큼 기다린 뒤 답한다(저장소 지연).
    pub fn delay(&self, delay: std::time::Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    /// 저장소의 활성 집합 판본을 바꾼다(None은 아직 게시된 적 없음).
    pub fn set(&self, current: Option<&str>) {
        *self.current.lock().unwrap() = current.map(|r| Revision(r.to_owned()));
    }

    /// 이 프로세스가 `expected` 집합으로 시작한 것처럼 저장소의 활성 집합과 견주는 확인
    pub fn check(self: &Arc<Self>, expected: &str) -> Arc<dyn ActiveCheck> {
        Arc::new(ActiveSet::new(self.clone(), Revision(expected.to_owned())))
    }
}

impl ActiveRevisions for MemoryActive {
    fn current(&self) -> Reply<'_, Option<Revision>> {
        let delay = *self.delay.lock().unwrap();
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            Ok(self.current.lock().unwrap().clone())
        })
    }

    fn replace(
        &self,
        expected: Option<Revision>,
        next: Revision,
        profiles: BTreeMap<ProfileId, Revision>,
        _: Vec<MemorySeed>,
        schedules: BTreeMap<ProfileId, Vec<TriggerSpec>>,
    ) -> Reply<'_, ()> {
        let mut current = self.current.lock().unwrap();
        // 계약: 집합 밖 프로필의 예약은 Invalid, 기대 판본이 다르면 Conflict
        let result = if schedules
            .keys()
            .any(|profile| !profiles.contains_key(profile))
        {
            Err(Error::Invalid)
        } else if *current == expected {
            *current = Some(next);
            Ok(())
        } else {
            Err(Error::Conflict)
        };
        Box::pin(std::future::ready(result))
    }
}

/// 봇 참여 스레드 증거 저장소(Slack `thread_reply`용). 시험이 기록을 읽고 읽기 실패를 만든다.
#[derive(Default)]
pub struct MemoryThreads {
    inner: Mutex<ThreadsInner>,
}

#[derive(Default)]
struct ThreadsInner {
    known: std::collections::BTreeSet<String>,
    notes: Vec<bot_core::slack::BotThread>,
    fail_reads: bool,
    fail_writes: bool,
}

fn thread_text(t: &bot_core::slack::BotThread) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}\u{1f}{}",
        t.profile.0, t.binding.0, t.team, t.bot, t.channel, t.root_ts
    )
}

impl MemoryThreads {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 지금까지 기록하려고 한 스레드들(중복 포함, 기록 순서)
    pub fn notes(&self) -> Vec<bot_core::slack::BotThread> {
        self.inner.lock().unwrap().notes.clone()
    }

    /// 읽기(`known`)가 `Unavailable`로 실패하게 한다(저장소 장애).
    pub fn fail_reads(&self, on: bool) {
        self.inner.lock().unwrap().fail_reads = on;
    }

    /// 쓰기(`note`)가 실패하게 한다.
    pub fn fail_writes(&self, on: bool) {
        self.inner.lock().unwrap().fail_writes = on;
    }

    /// 이미 기록된 것으로 만든다(전환 전 열린 스레드를 증거로 채운 것처럼).
    pub fn seed(&self, thread: &bot_core::slack::BotThread) {
        self.inner.lock().unwrap().known.insert(thread_text(thread));
    }
}

impl bot_core::slack::BotThreads for MemoryThreads {
    fn note(&self, thread: bot_core::slack::BotThread) -> bot_core::port::Reply<'_, ()> {
        let mut inner = self.inner.lock().unwrap();
        let result = if inner.fail_writes {
            Err(bot_core::port::Error::Unavailable)
        } else {
            inner.known.insert(thread_text(&thread));
            inner.notes.push(thread);
            Ok(())
        };
        Box::pin(std::future::ready(result))
    }

    fn known(&self, thread: bot_core::slack::BotThread) -> bot_core::port::Reply<'_, bool> {
        let inner = self.inner.lock().unwrap();
        let result = if inner.fail_reads {
            Err(bot_core::port::Error::Unavailable)
        } else {
            Ok(inner.known.contains(&thread_text(&thread)))
        };
        Box::pin(std::future::ready(result))
    }
}
