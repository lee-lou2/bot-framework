//! 모델 샌드박스의 시험용 가짜: 각본대로 출력하는 `FakeSandbox`. 이 크레이트의 시험(`cfg(test)`)에서만 컴파일되고 다른 크레이트는 쓰지 않는다.
//! 포트(`Sandbox<ModelSpec>`·`ModelProcess`·`ModelSpec`)와 구현이 지킬 계약은 `bot_core::sandbox`가 소유한다.

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

/// 가짜 프로세스 한 번의 각본.
#[derive(Clone)]
pub struct Script {
    pub lines: Vec<String>,
    pub exit: Exit,
    /// 줄마다 기다리는 시간
    pub delay: Duration,
    /// 줄을 다 낸 뒤 끝나지 않고 멈춘다(`kill`되면 끝난다)
    pub hang: bool,
    /// 있으면 시작을 이 코드로 거절한다
    pub refuse: Option<&'static str>,
}

impl Script {
    /// 표본 출력의 줄들을 내고 종료 코드 0으로 끝난다.
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

/// 각본대로 출력하는 가짜 샌드박스. 시작마다 각본 하나를 쓰고, 받은 명세를 기록한다.
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

    /// 시작마다 명세를 들여다보는 훅(호출 폴더가 아직 있을 때 파일 내용을 확인하는 시험용).
    pub fn inspect(self, hook: impl Fn(&ModelSpec) + Send + Sync + 'static) -> Self {
        self.lock().hook = Some(Arc::new(hook));
        self
    }

    /// 지금까지 받은 명세(시작 순서)
    pub fn specs(&self) -> Vec<ModelSpec> {
        self.lock().started.clone()
    }

    /// `kill`이 불린 프로세스 수
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

/// 시험용: `roots` 아래 모든 파일에서 `needle`을 이름이나 내용에 담은 것의 경로를 모은다(심볼릭 링크는 따라가지 않는다).
/// 모델이 읽을 수 있는 폴더 전체를 훑어 비밀 값이 0건인지 확인하는 데 쓴다.
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
    assert!(!needle.is_empty(), "빈 값은 훑을 수 없다");
    let mut hits = Vec::new();
    for root in roots {
        walk(root, needle, &mut hits);
    }
    hits
}
