//! Scripted test double (`FakeSandbox`) and file-tree secret scanner for `bot-runtime` tests.

use bot_core::{
    port::{Error, Reply},
    sandbox::{ExitFuture, LineFuture, ModelProcess, ModelSpec, Sandbox, UnitFuture},
};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub use bot_core::sandbox::Exit;

#[derive(Clone)]
pub struct Script {
    pub lines: Vec<String>,
    pub exit: Exit,
    pub delay: Duration,
    pub hang: bool,
    pub refuse: Option<&'static str>,
}

impl Script {
    pub fn output(sample: &str) -> Script {
        Script {
            lines: sample.lines().map(str::to_owned).collect(),
            exit: Exit {
                code: Some(0),
                stderr_tail: String::new(),
            },
            delay: Duration::ZERO,
            hang: false,
            refuse: None,
        }
    }

    pub fn exits(mut self, code: i32, stderr: &str) -> Script {
        self.exit = Exit {
            code: Some(code),
            stderr_tail: stderr.to_owned(),
        };
        self
    }

    pub fn hangs(mut self) -> Script {
        self.hang = true;
        self
    }

    pub fn slow(mut self, delay: Duration) -> Script {
        self.delay = delay;
        self
    }

    pub fn refused(code: &'static str) -> Script {
        Script {
            refuse: Some(code),
            ..Script::output("")
        }
    }
}

type Hook = Arc<dyn Fn(&ModelSpec) + Send + Sync>;

#[derive(Default)]
struct FakeState {
    scripts: VecDeque<Script>,
    started: Vec<ModelSpec>,
    killed: usize,
    hook: Option<Hook>,
}

#[derive(Clone, Default)]
pub struct FakeSandbox {
    state: Arc<Mutex<FakeState>>,
}

impl FakeSandbox {
    pub fn new(scripts: impl IntoIterator<Item = Script>) -> Self {
        let fake = FakeSandbox::default();
        fake.lock().scripts.extend(scripts);
        fake
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn inspect(self, hook: impl Fn(&ModelSpec) + Send + Sync + 'static) -> Self {
        self.lock().hook = Some(Arc::new(hook));
        self
    }

    pub fn specs(&self) -> Vec<ModelSpec> {
        self.lock().started.clone()
    }

    pub fn killed(&self) -> usize {
        self.lock().killed
    }
}

struct FakeProcess {
    lines: VecDeque<String>,
    exit: Exit,
    delay: Duration,
    hang: bool,
    killed: bool,
    state: Arc<Mutex<FakeState>>,
}

impl Sandbox<ModelSpec> for FakeSandbox {
    type Process = Box<dyn ModelProcess>;

    fn spawn(&self, spec: ModelSpec) -> Reply<'_, Self::Process> {
        Box::pin(async move {
            let hook = self.lock().hook.clone();
            if let Some(hook) = hook {
                hook(&spec);
            }
            let mut state = self.lock();
            state.started.push(spec);
            let Some(script) = state.scripts.pop_front() else {
                return Err(Error::Unavailable);
            };
            if script.refuse.is_some() {
                return Err(Error::Unavailable);
            }
            let process = FakeProcess {
                lines: script.lines.into(),
                exit: script.exit,
                delay: script.delay,
                hang: script.hang,
                killed: false,
                state: self.state.clone(),
            };
            Ok(Box::new(process) as Box<dyn ModelProcess>)
        })
    }

    fn reap(&self, _owner: &str) -> Reply<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

impl ModelProcess for FakeProcess {
    fn next_line(&mut self) -> LineFuture<'_> {
        Box::pin(async move {
            if self.killed {
                return None;
            }
            if !self.lines.is_empty() {
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                return self.lines.pop_front();
            }
            if self.hang {
                std::future::pending::<()>().await;
            }
            None
        })
    }

    fn kill(&mut self) -> UnitFuture<'_> {
        Box::pin(async move {
            if !self.killed {
                self.killed = true;
                self.state.lock().unwrap_or_else(|e| e.into_inner()).killed += 1;
            }
        })
    }

    fn wait(&mut self) -> ExitFuture<'_> {
        Box::pin(async move {
            if self.killed {
                Exit {
                    code: None,
                    stderr_tail: String::new(),
                }
            } else {
                self.exit.clone()
            }
        })
    }
}

/// Scans `roots` without following symlinks for any file whose path or content contains `needle`.
pub(crate) fn files_holding(roots: &[&Path], needle: &str) -> Vec<PathBuf> {
    fn walk(dir: &Path, needle: &str, hits: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if path.to_string_lossy().contains(needle) {
                hits.push(path.clone());
            }
            if kind.is_dir() {
                walk(&path, needle, hits);
            } else if kind.is_file()
                && std::fs::read(&path)
                    .is_ok_and(|bytes| bytes.windows(needle.len()).any(|w| w == needle.as_bytes()))
            {
                hits.push(path);
            }
        }
    }
    assert!(!needle.is_empty(), "needle cannot be empty");
    let mut hits = Vec::new();
    for root in roots {
        walk(root, needle, &mut hits);
    }
    hits
}
