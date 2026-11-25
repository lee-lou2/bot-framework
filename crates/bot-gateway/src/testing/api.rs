//! OpenAI 호환 입구 시험용 메모리 `ApiRuns`·`ProfileBundles`. 접수·재전달·한도·충돌은 저장소 계약대로 흉내 내고, 시험이 결함(커밋 전 실패·응답 유실·정체)과
//! 작업의 진행(완료·승인 대기·실패)을 직접 심는다. 운영 빌드에는 넣지 않는다(`testing` 기능).
use bot_core::{
    api::{ApiAccept, ApiAdmission, ApiKey, ApiRun, ApiRuns, ApiState},
    command::Target,
    id::{AgentId, ProfileId, RunId},
    port::{Error, Reply},
    profile::ProfileBundles,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

/// 접수 호출 하나에 심는 결함(다음 호출 하나에만 쓰인다)
#[derive(Clone, Copy, Debug)]
pub enum Fault {
    /// 확실히 커밋 전 실패(`Err(Unavailable)`)
    Unavailable,
    /// 활성 집합 불일치(`Stale`)
    Stale,
    /// 커밋했고 응답만 잃었다(`Unknown`) — 같은 키 조회가 작업을 찾는다
    LostAfterCommit,
    /// 커밋하지 못했는데 모른다고 답한다(`Unknown`) — 같은 키 조회는 없음이다
    UnknownNotCommitted,
    /// 이 시간만큼 멈춘 뒤 정상으로 처리한다(기한 시험)
    Hang(Duration),
}

struct Row {
    run: ApiRun,
    endpoint: String,
}

struct State {
    rows: BTreeMap<String, Row>,
    /// 새 작업이 시작하는 상태(기본 `Active` — 시험이 worker의 완료를 직접 심는다)
    start: ApiState,
    created: u64,
    faults: VecDeque<Fault>,
    lookup_fails: bool,
    /// 조회가 답하기 전에 기다리는 시간(저장소가 멈춘 상황)
    lookup_delay: Duration,
    /// 끝나기 전에 버려진 조회 수(상대가 끊거나 기한이 지나 취소됐다)
    lookups_dropped: u32,
    accepts: Vec<ApiAccept>,
    lookups: u32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            rows: BTreeMap::new(),
            start: ApiState::Active,
            created: 0,
            faults: VecDeque::new(),
            lookup_fails: false,
            lookup_delay: Duration::ZERO,
            lookups_dropped: 0,
            accepts: Vec::new(),
            lookups: 0,
        }
    }
}

#[derive(Default)]
pub struct MemoryApi {
    state: Mutex<State>,
}

fn text(profile: &str, endpoint: &str, principal: &str, key: &str) -> String {
    format!("{profile}\u{1f}{endpoint}\u{1f}{principal}\u{1f}{key}")
}

fn live(state: &ApiState) -> bool {
    matches!(
        state,
        ApiState::Active | ApiState::NeedsHuman | ApiState::Held
    )
}

impl MemoryApi {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 다음 접수 호출에 결함을 심는다(여럿이면 차례로).
    pub fn fault(&self, fault: Fault) {
        self.state.lock().unwrap().faults.push_back(fault);
    }

    /// 앞으로 새로 만드는 작업이 이 상태로 시작한다(worker가 곧바로 끝낸 것을 흉내 낸다).
    pub fn start_as(&self, state: ApiState) {
        self.state.lock().unwrap().start = state;
    }

    /// 조회가 계속 실패한다(`Err(Unavailable)`)
    pub fn fail_lookups(&self, fail: bool) {
        self.state.lock().unwrap().lookup_fails = fail;
    }

    /// 이 멱등 키로 접수한 작업의 상태를 바꾼다(worker가 끝낸 것을 흉내 낸다). 키는 시험 안에서 유일해야 한다.
    pub fn set_state(&self, request: &str, state: ApiState) {
        let mut inner = self.state.lock().unwrap();
        let row = inner
            .rows
            .iter_mut()
            .find(|(name, _)| name.ends_with(&format!("\u{1f}{request}")))
            .map(|(_, row)| row)
            .expect("그 키로 접수한 작업이 있어야 해요");
        row.run.state = state;
    }

    /// 새로 만든 작업 수(재전달은 세지 않는다)
    pub fn runs(&self) -> usize {
        self.state.lock().unwrap().rows.len()
    }

    /// 접수 호출을 모두 기록한 것(재전달·충돌·거절 포함)
    pub fn accepts(&self) -> Vec<ApiAccept> {
        self.state.lock().unwrap().accepts.clone()
    }

    /// 앞으로의 조회가 이 시간만큼 기다린 뒤 답한다
    pub fn delay_lookups(&self, delay: Duration) {
        self.state.lock().unwrap().lookup_delay = delay;
    }

    /// 답하기 전에 버려진 조회 수
    pub fn dropped_lookups(&self) -> u32 {
        self.state.lock().unwrap().lookups_dropped
    }

    pub fn lookups(&self) -> u32 {
        self.state.lock().unwrap().lookups
    }

    /// 이 범위의 작업이 이미 있는 것으로 심는다(접수 호출 없이 조회만 시험할 때)
    pub fn seed(&self, key: &ApiKey, run: ApiRun) {
        let name = text(&key.profile.0, &key.endpoint, &key.principal.0, &key.key.0);
        self.state.lock().unwrap().rows.insert(
            name,
            Row {
                run,
                endpoint: key.endpoint.clone(),
            },
        );
    }

    /// 이 멱등 키의 작업을 원장에서 지운다(조회가 «없음»을 돌려주는 상황)
    pub fn forget(&self, request: &str) {
        self.state
            .lock()
            .unwrap()
            .rows
            .retain(|name, _| !name.ends_with(&format!("\u{1f}{request}")));
    }

    pub fn run_of(&self, request: &str) -> Option<ApiRun> {
        let inner = self.state.lock().unwrap();
        inner
            .rows
            .iter()
            .find(|(name, _)| name.ends_with(&format!("\u{1f}{request}")))
            .map(|(_, row)| row.run.clone())
    }

    /// 이 키로 접수한 작업을 다른 모델·바인딩·에이전트로 바꿔 놓는다(재지정 흉내)
    pub fn reassign(&self, request: &str, binding: &str, agent: &str) {
        let mut inner = self.state.lock().unwrap();
        let row = inner
            .rows
            .iter_mut()
            .find(|(name, _)| name.ends_with(&format!("\u{1f}{request}")))
            .map(|(_, row)| row)
            .expect("그 키로 접수한 작업이 있어야 해요");
        row.run.binding = bot_core::id::BindingId(binding.to_owned());
        row.run.agent = AgentId(agent.to_owned());
    }
}

impl ApiRuns for MemoryApi {
    fn accept(&self, request: ApiAccept) -> Reply<'_, ApiAdmission> {
        Box::pin(async move {
            let fault = {
                let mut inner = self.state.lock().unwrap();
                inner.accepts.push(request.clone());
                inner.faults.pop_front()
            };
            if let Some(Fault::Hang(wait)) = fault {
                tokio::time::sleep(wait).await;
            }
            match fault {
                Some(Fault::Unavailable) => return Err(Error::Unavailable),
                Some(Fault::Stale) => return Ok(ApiAdmission::Stale),
                Some(Fault::UnknownNotCommitted) => return Ok(ApiAdmission::Unknown),
                _ => {}
            }
            let event = &request.accept.event;
            let name = text(
                &event.profile.0,
                &request.endpoint,
                &event.principal.0,
                &event.request.0,
            );
            let mut inner = self.state.lock().unwrap();
            if let Some(row) = inner.rows.get(&name) {
                let reply = if row.run.digest == request.digest {
                    ApiAdmission::Replayed(row.run.clone())
                } else {
                    ApiAdmission::Conflict
                };
                return Ok(reply);
            }
            let active = inner
                .rows
                .values()
                .filter(|row| row.endpoint == request.endpoint && live(&row.run.state))
                .count();
            if active >= request.max_active as usize {
                return Ok(ApiAdmission::Capacity);
            }
            inner.created += 1;
            let n = inner.created;
            let agent = match &request.accept.target {
                Target::Agent { agent, .. } => agent.clone(),
                Target::Tool { agent, .. } => agent.clone(),
            };
            let run = ApiRun {
                run: RunId(format!("00000000-0000-4000-8000-{n:012}")),
                created: 1_700_000_000 + n,
                model: request.model.clone(),
                binding: event.binding.clone(),
                agent,
                revision: request.accept.revision.clone(),
                digest: request.digest,
                state: inner.start.clone(),
            };
            inner.rows.insert(
                name,
                Row {
                    run: run.clone(),
                    endpoint: request.endpoint.clone(),
                },
            );
            if matches!(fault, Some(Fault::LostAfterCommit)) {
                return Ok(ApiAdmission::Unknown);
            }
            Ok(ApiAdmission::Accepted(run))
        })
    }

    fn lookup(&self, key: ApiKey) -> Reply<'_, Option<ApiRun>> {
        Box::pin(async move {
            let delay = {
                let mut inner = self.state.lock().unwrap();
                inner.lookups += 1;
                inner.lookup_delay
            };
            if !delay.is_zero() {
                // 기다리는 동안 이 호출이 버려지면(취소) 센다
                let guard = DropCount(&self.state, true);
                tokio::time::sleep(delay).await;
                guard.finish();
            }
            let inner = self.state.lock().unwrap();
            if inner.lookup_fails {
                return Err(Error::Unavailable);
            }
            let name = text(&key.profile.0, &key.endpoint, &key.principal.0, &key.key.0);
            Ok(inner.rows.get(&name).map(|row| row.run.clone()))
        })
    }
}

/// 끝나기 전에 버려지면 «버려진 조회» 수를 올리는 표지
struct DropCount<'a>(&'a Mutex<State>, bool);

impl DropCount<'_> {
    fn finish(mut self) {
        self.1 = false;
    }
}

impl Drop for DropCount<'_> {
    fn drop(&mut self) {
        if self.1 {
            self.0.lock().unwrap().lookups_dropped += 1;
        }
    }
}

/// `created_ms`만 답하는 메모리 `ProfileBundles`. 시험이 값을 정하거나(`None`=모름) 실패시킨다.
pub struct MemoryBundles {
    created_ms: Mutex<Option<i64>>,
    fail: Mutex<bool>,
    lookups: Mutex<BTreeSet<(String, String)>>,
    delay: Mutex<Duration>,
}

impl MemoryBundles {
    pub fn new(created_ms: Option<i64>) -> Arc<Self> {
        Arc::new(Self {
            created_ms: Mutex::new(created_ms),
            fail: Mutex::new(false),
            lookups: Mutex::new(BTreeSet::new()),
            delay: Mutex::new(Duration::ZERO),
        })
    }

    pub fn set(&self, created_ms: Option<i64>) {
        *self.created_ms.lock().unwrap() = created_ms;
    }

    pub fn fail(&self, fail: bool) {
        *self.fail.lock().unwrap() = fail;
    }

    /// `created_ms`가 답하기 전에 기다리는 시간(저장소 지연)
    pub fn delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    /// 물어 본 (프로필, 판본)들
    pub fn asked(&self) -> BTreeSet<(String, String)> {
        self.lookups.lock().unwrap().clone()
    }
}

impl ProfileBundles for MemoryBundles {
    fn put_bundle(&self, _: ProfileId, _: bot_core::id::Revision, _: Vec<u8>) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }

    fn bundle(&self, _: &ProfileId, _: &bot_core::id::Revision) -> Reply<'_, Option<Vec<u8>>> {
        Box::pin(async { Ok(None) })
    }

    fn created_ms(
        &self,
        profile: &ProfileId,
        revision: &bot_core::id::Revision,
    ) -> Reply<'_, Option<i64>> {
        self.lookups
            .lock()
            .unwrap()
            .insert((profile.0.clone(), revision.0.clone()));
        let fail = *self.fail.lock().unwrap();
        let value = *self.created_ms.lock().unwrap();
        let delay = *self.delay.lock().unwrap();
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if fail {
                Err(Error::Unavailable)
            } else {
                Ok(value)
            }
        })
    }

    fn pinned_revisions<'a>(
        &'a self,
        _: &'a BTreeMap<ProfileId, bot_core::id::Revision>,
    ) -> Reply<'a, BTreeSet<(ProfileId, bot_core::id::Revision)>> {
        Box::pin(async { Ok(BTreeSet::new()) })
    }
}
