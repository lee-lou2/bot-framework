//! In-memory `ApiRuns` and `ProfileBundles` test doubles with fault injection for OpenAI gateway tests.
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

#[derive(Clone, Copy, Debug)]
pub enum Fault {
    Unavailable,
    Stale,
    /// Commits the row then returns `Unknown`, simulating a lost response after commit.
    LostAfterCommit,
    /// Returns `Unknown` without committing a row.
    UnknownNotCommitted,
    Hang(Duration),
}

struct Row {
    run: ApiRun,
    endpoint: String,
}

struct State {
    rows: BTreeMap<String, Row>,
    start: ApiState,
    created: u64,
    faults: VecDeque<Fault>,
    lookup_fails: bool,
    lookup_delay: Duration,
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

    pub fn fault(&self, fault: Fault) {
        self.state.lock().unwrap().faults.push_back(fault);
    }

    pub fn start_as(&self, state: ApiState) {
        self.state.lock().unwrap().start = state;
    }

    pub fn fail_lookups(&self, fail: bool) {
        self.state.lock().unwrap().lookup_fails = fail;
    }

    pub fn set_state(&self, request: &str, state: ApiState) {
        let mut inner = self.state.lock().unwrap();
        let row = inner
            .rows
            .iter_mut()
            .find(|(name, _)| name.ends_with(&format!("\u{1f}{request}")))
            .map(|(_, row)| row)
            .expect("expected an accepted run for key");
        row.run.state = state;
    }

    pub fn runs(&self) -> usize {
        self.state.lock().unwrap().rows.len()
    }

    pub fn accepts(&self) -> Vec<ApiAccept> {
        self.state.lock().unwrap().accepts.clone()
    }

    pub fn delay_lookups(&self, delay: Duration) {
        self.state.lock().unwrap().lookup_delay = delay;
    }

    pub fn dropped_lookups(&self) -> u32 {
        self.state.lock().unwrap().lookups_dropped
    }

    pub fn lookups(&self) -> u32 {
        self.state.lock().unwrap().lookups
    }

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

    pub fn reassign(&self, request: &str, binding: &str, agent: &str) {
        let mut inner = self.state.lock().unwrap();
        let row = inner
            .rows
            .iter_mut()
            .find(|(name, _)| name.ends_with(&format!("\u{1f}{request}")))
            .map(|(_, row)| row)
            .expect("expected an accepted run for key");
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

    pub fn delay(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

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
