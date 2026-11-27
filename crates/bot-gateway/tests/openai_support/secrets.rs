//! Test `SecretStore` supporting fault injection and per-read hooks.
use bot_core::{
    id::ProfileId,
    port::{Error, ProfileSecrets, Reply, SecretStore},
};
use bot_gateway::testing::MemorySecrets;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

type Hook = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

pub struct FlakySecrets {
    inner: Arc<MemorySecrets>,
    broken: AtomicBool,
    /// Fails when acquiring the profile handle before any secret lookup.
    no_handle: AtomicBool,
    /// Hook invoked before each secret resolution (simulates concurrent publication).
    on_read: Hook,
    /// Total store touches: handle opens plus secret resolutions.
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

    pub fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }

    pub fn on_read(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_read.lock().unwrap() = Some(Arc::new(hook));
    }

    pub fn break_it(&self, broken: bool) {
        self.broken.store(broken, Ordering::SeqCst);
    }

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
