use crate::ports::*;
use bot_core::{
    command::*,
    id::*,
    port::{AuditLog, EffectJournal, EffectPermit, Error, Reply},
    profile::{ProfileCatalog, Snapshot},
    state::EffectState,
    trace::{SpanRecord, Tracer},
};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use super::lock;

#[derive(Default)]
pub struct FakeCatalog {
    inner: Mutex<CatalogState>,
}

#[derive(Default)]
struct CatalogState {
    active: BTreeMap<ProfileId, Arc<Snapshot>>,
    by_revision: HashMap<(ProfileId, Revision), Arc<Snapshot>>,
}

impl FakeCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish(&self, snapshot: Arc<Snapshot>) {
        let mut state = lock(&self.inner);
        state.by_revision.insert(
            (snapshot.id.clone(), snapshot.revision.clone()),
            Arc::clone(&snapshot),
        );
        state.active.insert(snapshot.id.clone(), snapshot);
    }

    pub fn activate(&self, profile: &str, revision: &str) {
        let mut state = lock(&self.inner);
        let key = (ProfileId(profile.into()), Revision(revision.into()));
        let snapshot = state
            .by_revision
            .get(&key)
            .cloned()
            .expect("published revision");
        state.active.insert(key.0, snapshot);
    }

    pub fn retire(&self, profile: &str) {
        lock(&self.inner).active.remove(&ProfileId(profile.into()));
    }
}

impl ProfileCatalog for FakeCatalog {
    fn active(&self) -> Arc<BTreeMap<ProfileId, Arc<Snapshot>>> {
        Arc::new(lock(&self.inner).active.clone())
    }

    fn lookup(&self, profile: &ProfileId, revision: &Revision) -> Option<Arc<Snapshot>> {
        lock(&self.inner)
            .by_revision
            .get(&(profile.clone(), revision.clone()))
            .cloned()
    }
}

pub struct FakeRunner {
    jobs: Mutex<Vec<ToolJob>>,
    script: Mutex<VecDeque<Result<Ran, Error>>>,
    default_result: Mutex<String>,
    reaped: Mutex<Vec<String>>,
    reap_ok: Mutex<bool>,
    images: Mutex<HashMap<String, String>>,
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    on_run: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    refused: Mutex<Vec<String>>,
    missing_secrets: Mutex<Vec<String>>,
    secret_failure: Mutex<Option<Error>>,
    secret_checks: Mutex<Vec<(ProfileId, Vec<String>)>>,
    server_secrets: Mutex<HashMap<String, Vec<String>>>,
    unproven: Mutex<Vec<String>>,
}

impl Default for FakeRunner {
    fn default() -> Self {
        Self {
            jobs: Mutex::default(),
            script: Mutex::default(),
            default_result: Mutex::new("{\"ok\":true}".to_owned()),
            reaped: Mutex::default(),
            reap_ok: Mutex::new(true),
            images: Mutex::new([("demo-agenda".to_owned(), "img1".to_owned())].into()),
            gate: Mutex::new(None),
            on_run: Mutex::new(None),
            refused: Mutex::new(Vec::new()),
            missing_secrets: Mutex::new(Vec::new()),
            secret_failure: Mutex::new(None),
            secret_checks: Mutex::new(Vec::new()),
            server_secrets: Mutex::new(HashMap::new()),
            unproven: Mutex::new(Vec::new()),
        }
    }
}

impl FakeRunner {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn script(&self, ran: Ran) {
        lock(&self.script).push_back(Ok(ran));
    }
    pub fn set_default_result(&self, result: &str) {
        *lock(&self.default_result) = result.to_owned();
    }
    /// Pauses the next `run` until the returned semaphore receives a permit (after recording the job).
    pub fn hold_next(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *lock(&self.gate) = Some(gate.clone());
        gate
    }
    pub fn refuse_server(&self, server: &str) {
        lock(&self.refused).push(server.to_owned());
    }
    pub fn miss_secret(&self, name: &str) {
        lock(&self.missing_secrets).push(name.to_owned());
    }
    pub fn restore_secret(&self, name: &str) {
        lock(&self.missing_secrets).retain(|missing| missing != name);
    }
    pub fn fail_secret_checks_with(&self, error: Option<Error>) {
        *lock(&self.secret_failure) = error;
    }
    pub fn secret_checks(&self) -> Vec<(ProfileId, Vec<String>)> {
        lock(&self.secret_checks).clone()
    }
    pub fn on_run(&self, hook: impl Fn() + Send + Sync + 'static) {
        *lock(&self.on_run) = Some(Arc::new(hook));
    }
    pub fn fail_next(&self) {
        lock(&self.script).push_back(Err(Error::Unavailable));
    }
    pub fn jobs(&self) -> Vec<ToolJob> {
        lock(&self.jobs).clone()
    }
    pub fn reaped(&self) -> Vec<String> {
        lock(&self.reaped).clone()
    }
    pub fn set_reap_ok(&self, ok: bool) {
        *lock(&self.reap_ok) = ok;
    }
    pub fn bind_server_secret(&self, server: &str, name: &str) {
        lock(&self.server_secrets)
            .entry(server.to_owned())
            .or_default()
            .push(name.to_owned());
    }
    /// Marks `server` so a successful `reap` does not prove termination (e.g., remote HTTP servers).
    pub fn reap_proves_nothing_for(&self, server: &str) {
        lock(&self.unproven).push(server.to_owned());
    }
    pub fn set_image(&self, server: &str, image: Option<&str>) {
        let mut images = lock(&self.images);
        match image {
            Some(image) => images.insert(server.into(), image.into()),
            None => images.remove(server),
        };
    }
}

impl ToolRunner for FakeRunner {
    fn installed(&self, server: &str) -> Option<String> {
        lock(&self.images).get(server).cloned()
    }

    fn admit(&self, server: &str) -> Result<(), Error> {
        if lock(&self.refused).iter().any(|s| s == server) {
            Err(Error::Denied)
        } else {
            Ok(())
        }
    }

    fn server_secrets(&self, server: &str) -> Vec<SecretName> {
        lock(&self.server_secrets)
            .get(server)
            .map(|names| names.iter().filter_map(|n| SecretName::parse(n)).collect())
            .unwrap_or_default()
    }

    fn reap_proves_end(&self, server: &str) -> bool {
        !lock(&self.unproven).iter().any(|s| s == server)
    }

    fn admit_secrets(&self, profile: &ProfileId, names: &[SecretName]) -> Reply<'_, ()> {
        let names: Vec<String> = names.iter().map(|n| n.as_str().to_owned()).collect();
        let missing = lock(&self.missing_secrets)
            .iter()
            .any(|miss| names.contains(miss));
        lock(&self.secret_checks).push((profile.clone(), names));
        let failure = lock(&self.secret_failure).is_some();
        Box::pin(async move {
            if failure {
                Err(Error::Unavailable)
            } else if missing {
                Err(Error::Missing)
            } else {
                Ok(())
            }
        })
    }

    fn run(&self, job: ToolJob) -> Reply<'_, Ran> {
        lock(&self.jobs).push(job);
        let hook = lock(&self.on_run).clone();
        if let Some(hook) = hook {
            hook();
        }
        let ran = lock(&self.script)
            .pop_front()
            .unwrap_or_else(|| Ok(Ran::Done(JsonPayload(lock(&self.default_result).clone()))));
        let gate = lock(&self.gate).take();
        Box::pin(async move {
            if let Some(gate) = gate {
                let _ = gate.acquire().await;
            }
            ran
        })
    }

    fn reap(&self, owner: String) -> Reply<'_, ()> {
        lock(&self.reaped).push(owner);
        let ok = *lock(&self.reap_ok);
        Box::pin(async move { if ok { Ok(()) } else { Err(Error::Unavailable) } })
    }
}

#[derive(Default)]
pub struct FakeEffects {
    unresolved: Mutex<Vec<(String, EffectState, Option<String>)>>,
    fail: Mutex<bool>,
}

impl FakeEffects {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn set_unresolved(&self, steps: &[(&str, EffectState, Option<&str>)]) {
        *lock(&self.unresolved) = steps
            .iter()
            .map(|(id, state, why)| (id.to_string(), *state, why.map(str::to_owned)))
            .collect();
    }
    pub fn set_unavailable(&self, fail: bool) {
        *lock(&self.fail) = fail;
    }
}

impl EffectJournal for FakeEffects {
    fn request(&self, _: EffectRequest) -> Reply<'_, EffectPermit> {
        Box::pin(async { Err(Error::Unavailable) })
    }

    fn lookup(&self, _: EffectRequest, _: ToolId) -> Reply<'_, Option<EffectStatus>> {
        Box::pin(async { Err(Error::Unavailable) })
    }

    fn status(&self, _: Attempt, _: CallId, _: EffectId) -> Reply<'_, EffectStatus> {
        Box::pin(async { Err(Error::Unavailable) })
    }

    fn unresolved(&self, _: Attempt, _: CallId) -> Reply<'_, Vec<EffectStatus>> {
        let steps: Vec<EffectStatus> = lock(&self.unresolved)
            .iter()
            .map(|(id, state, receipt)| EffectStatus {
                effect: EffectId(id.clone()),
                state: *state,
                resolution: None,
                receipt: receipt.clone(),
            })
            .collect();
        let fail = *lock(&self.fail);
        Box::pin(async move {
            if fail {
                Err(Error::Unavailable)
            } else {
                Ok(steps)
            }
        })
    }

    fn record(&self, _: Attempt, _: CallId, _: EffectStatus) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

#[derive(Default)]
pub struct MemAudit {
    entries: Mutex<Vec<AuditEntry>>,
}

impl MemAudit {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn entries(&self) -> Vec<AuditEntry> {
        lock(&self.entries).clone()
    }
    pub fn verdicts(&self) -> Vec<(String, String)> {
        lock(&self.entries)
            .iter()
            .map(|e| (e.action.clone(), e.verdict.clone()))
            .collect()
    }
}

impl AuditLog for MemAudit {
    fn append(&self, entry: AuditEntry) -> Reply<'_, u64> {
        let mut entries = lock(&self.entries);
        entries.push(entry);
        let seq = entries.len() as u64;
        Box::pin(async move { Ok(seq) })
    }

    fn verify(&self) -> Reply<'_, ChainReport> {
        let entries = lock(&self.entries).len() as u64;
        Box::pin(async move {
            Ok(ChainReport {
                entries,
                head: [0; 32],
                first_bad: None,
            })
        })
    }
}

#[derive(Default)]
pub struct RecordingTracer {
    spans: Mutex<Vec<SpanRecord>>,
}

impl RecordingTracer {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn spans(&self) -> Vec<SpanRecord> {
        lock(&self.spans).clone()
    }
}

impl Tracer for RecordingTracer {
    fn record(&self, span: SpanRecord) {
        lock(&self.spans).push(span);
    }
}
