//! 읽기를 실패시키거나 읽는 도중에 일을 끼워 넣을 수 있는 시험용 비밀 보관소.
use bot_core::{
    id::ProfileId,
    port::{Error, ProfileSecrets, Reply, SecretStore},
};
use bot_gateway::testing::MemorySecrets;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// 읽을 때마다 먼저 부르는 동작(없을 수 있다)
type Hook = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// 읽기를 실패시킬 수 있는 보관소(보관소 장애 시험)
pub struct FlakySecrets {
    inner: Arc<MemorySecrets>,
    broken: AtomicBool,
    /// 프로필 핸들을 얻는 것부터 실패한다
    no_handle: AtomicBool,
    /// 값을 읽을 때마다 먼저 부르는 동작(읽는 사이에 다른 프로세스가 게시하는 상황)
    on_read: Hook,
    /// 보관소에 닿은 횟수: 프로필 핸들을 연 것 + 값을 읽은 것
    reads: Arc<AtomicUsize>,
}

impl FlakySecrets {
    pub fn new(inner: Arc<MemorySecrets>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            broken: AtomicBool::new(false),
            no_handle: AtomicBool::new(false),
            on_read: Arc::new(Mutex::new(None)),
            reads: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// 지금까지 보관소에 닿은 횟수(핸들 열기 + 값 읽기). 차단된 요청이 보관소를 읽지 않는지 본다.
    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    /// 값을 읽을 때마다 `hook`을 먼저 부른다(읽는 사이에 일어나는 일을 흉내 낸다). 한 번만 쓰려면 `hook`이 스스로 확인한다.
    pub fn on_read(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_read.lock().unwrap() = Some(Arc::new(hook));
    }

    pub fn break_it(&self, broken: bool) {
        self.broken.store(broken, Ordering::SeqCst);
    }

    /// 값을 읽기 전에 프로필 핸들을 여는 것부터 실패한다
    pub fn break_handle(&self, broken: bool) {
        self.no_handle.store(broken, Ordering::SeqCst);
    }

    pub fn set(&self, name: &str, value: &[u8]) {
        self.inner.set("personal", name, value);
    }

    pub fn remove(&self, name: &str) {
        self.inner.remove("personal", name);
    }
}

/// 읽을 때마다 `on_read` 동작을 먼저 부르는 핸들
struct Hooked {
    inner: Box<dyn ProfileSecrets>,
    hook: Hook,
    reads: Arc<AtomicUsize>,
}

impl ProfileSecrets for Hooked {
    fn resolve(&self, name: bot_core::id::SecretName) -> Reply<'_, bot_core::port::SecretValue> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let hook = self.hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook();
        }
        self.inner.resolve(name)
    }
}

struct Failing;

impl ProfileSecrets for Failing {
    fn resolve(&self, _: bot_core::id::SecretName) -> Reply<'_, bot_core::port::SecretValue> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

impl SecretStore for FlakySecrets {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.no_handle.load(Ordering::SeqCst) {
            return Box::pin(async { Err(Error::Unavailable) });
        }
        if self.broken.load(Ordering::SeqCst) {
            return Box::pin(async { Ok(Box::new(Failing) as Box<dyn ProfileSecrets>) });
        }
        let hook = self.on_read.clone();
        let reads = self.reads.clone();
        let inner = self.inner.for_profile(profile);
        Box::pin(async move {
            let handle = inner.await?;
            Ok(Box::new(Hooked {
                inner: handle,
                hook,
                reads,
            }) as Box<dyn ProfileSecrets>)
        })
    }
}
