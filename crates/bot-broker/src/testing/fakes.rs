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

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 프로필 판본을 손으로 게시하는 카탈로그. 게시하면 그 판본이 활성이 된다.
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

    /// 이미 게시한 옛 판본을 다시 활성으로 돌린다.
    pub fn activate(&self, profile: &str, revision: &str) {
        let mut state = lock(&self.inner);
        let key = (ProfileId(profile.into()), Revision(revision.into()));
        let snapshot = state.by_revision.get(&key).cloned().expect("게시한 판본");
        state.active.insert(key.0, snapshot);
    }

    /// 프로필을 활성 집합에서 뺀다(철회).
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

/// 실행한 일을 기록하고 미리 적어 둔 결과를 돌려주는 실행기.
pub struct FakeRunner {
    jobs: Mutex<Vec<ToolJob>>,
    script: Mutex<VecDeque<Result<Ran, Error>>>,
    default_result: Mutex<String>,
    reaped: Mutex<Vec<String>>,
    reap_ok: Mutex<bool>,
    images: Mutex<HashMap<String, String>>,
    /// 다음 실행을 이 문이 열릴 때까지 붙잡아 둔다(실행 중에 일어나는 일을 보려고).
    gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
    /// 실행이 시작될 때마다 부르는 함수(실행하는 동안 판본이 바뀌는 상황을 만들려고).
    on_run: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// `admit`이 거절하는 서버 별칭(상태 볼륨을 요구하는데 지원하지 않는 서버).
    refused: Mutex<Vec<String>>,
    /// 보관소에 없는 자격 이름(`admit_secrets`가 `Missing`으로 답한다).
    missing_secrets: Mutex<Vec<String>>,
    /// `admit_secrets`가 이 오류로 실패한다(보관소를 읽지 못함).
    secret_failure: Mutex<Option<Error>>,
    /// `admit_secrets`가 받은 (프로필, 이름들).
    secret_checks: Mutex<Vec<(ProfileId, Vec<String>)>>,
    /// 서버 별칭 → 호스트가 그 서버에 묶은 자격 이름들(`server_secrets`, 원격 서버의 bearer처럼).
    server_secrets: Mutex<HashMap<String, Vec<String>>>,
    /// `reap`이 성공해도 «끝났다»의 증명이 아닌 서버 별칭들(원격 서버).
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
    /// 다음 실행의 결과를 적어 둔다. 비어 있으면 `{"ok":true}`로 끝난다.
    pub fn script(&self, ran: Ran) {
        lock(&self.script).push_back(Ok(ran));
    }
    /// 스크립트가 비었을 때 돌려줄 결과를 바꾼다.
    pub fn set_default_result(&self, result: &str) {
        *lock(&self.default_result) = result.to_owned();
    }
    /// 다음 실행을 붙잡아 둔다. 돌려준 문에 `add_permits(1)`을 하면 풀린다(`jobs()`에는 실행이 시작될 때 이미 보인다).
    pub fn hold_next(&self) -> Arc<tokio::sync::Semaphore> {
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *lock(&self.gate) = Some(gate.clone());
        gate
    }
    /// 이 서버 별칭을 `admit`이 거절하게 한다(상태 볼륨을 요구하는데 지원하지 않는 서버).
    pub fn refuse_server(&self, server: &str) {
        lock(&self.refused).push(server.to_owned());
    }
    /// 이 자격 이름이 보관소에 없다(`admit_secrets`가 `Missing`).
    pub fn miss_secret(&self, name: &str) {
        lock(&self.missing_secrets).push(name.to_owned());
    }
    /// 이 자격 이름이 다시 보관소에 있다.
    pub fn restore_secret(&self, name: &str) {
        lock(&self.missing_secrets).retain(|missing| missing != name);
    }
    /// 이후 `admit_secrets`가 이 오류로 실패한다(보관소를 읽지 못함, `None`이면 다시 정상).
    pub fn fail_secret_checks_with(&self, error: Option<Error>) {
        *lock(&self.secret_failure) = error;
    }
    /// `admit_secrets`가 받은 (프로필, 이름들).
    pub fn secret_checks(&self) -> Vec<(ProfileId, Vec<String>)> {
        lock(&self.secret_checks).clone()
    }
    /// 실행이 시작될 때마다 `hook`을 부른다(이 호출은 이미 `start`를 지났다).
    pub fn on_run(&self, hook: impl Fn() + Send + Sync + 'static) {
        *lock(&self.on_run) = Some(Arc::new(hook));
    }
    /// 다음 실행에서 실행기 자체가 실패한다.
    pub fn fail_next(&self) {
        lock(&self.script).push_back(Err(Error::Unavailable));
    }
    pub fn jobs(&self) -> Vec<ToolJob> {
        lock(&self.jobs).clone()
    }
    pub fn reaped(&self) -> Vec<String> {
        lock(&self.reaped).clone()
    }
    /// 옛 프로세스 종료를 확인할 수 있는가(기본 true).
    pub fn set_reap_ok(&self, ok: bool) {
        *lock(&self.reap_ok) = ok;
    }
    /// 이 서버로 가는 호출이 쓰는 호스트 쪽 자격 이름을 더한다(`server_secrets`).
    pub fn bind_server_secret(&self, server: &str, name: &str) {
        lock(&self.server_secrets)
            .entry(server.to_owned())
            .or_default()
            .push(name.to_owned());
    }
    /// 이 서버의 `reap` 성공은 옛 호출이 끝났다는 증명이 아니다(원격 서버처럼 연결을 닫아도 원격 실행은 이어질 수 있다).
    pub fn reap_proves_nothing_for(&self, server: &str) {
        lock(&self.unproven).push(server.to_owned());
    }
    /// 서버의 설치 판본을 바꾼다(`None`이면 미설치).
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

/// 미결 단계를 손으로 정하는 단계 원장. 단계 접수·기록은 `effects` 모듈의 시험이 따로 다룬다.
#[derive(Default)]
pub struct FakeEffects {
    unresolved: Mutex<Vec<(String, EffectState, Option<String>)>>,
    fail: Mutex<bool>,
}

impl FakeEffects {
    pub fn new() -> Self {
        Self::default()
    }
    /// 미결 단계(효과 ID, 상태, 사유)를 적어 둔다.
    pub fn set_unresolved(&self, steps: &[(&str, EffectState, Option<&str>)]) {
        *lock(&self.unresolved) = steps
            .iter()
            .map(|(id, state, why)| (id.to_string(), *state, why.map(str::to_owned)))
            .collect();
    }
    /// 단계 원장 조회가 실패하게 한다.
    pub fn set_unavailable(&self, fail: bool) {
        *lock(&self.fail) = fail;
    }
}

impl EffectJournal for FakeEffects {
    fn request(&self, _: EffectRequest) -> Reply<'_, EffectPermit> {
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

/// 단독 감사 기록을 모으는 가짜.
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
    /// `(action, verdict)` 쌍만 본다.
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

/// 끝난 스팬을 모으는 추적기.
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
