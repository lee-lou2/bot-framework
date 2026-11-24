//! 실제 CLI 시험의 «모델이 읽을 수 있는 폴더에 비밀 값이 없는가» 훑기: 프로세스가 끝난 직후(호출 폴더가 지워지기 전)에 마운트 폴더 전체를 훑는다.
#![allow(dead_code)]

use bot_core::{
    port::Reply,
    sandbox::{ExitFuture, LineFuture, ModelProcess, ModelSpec, Sandbox, UnitFuture},
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// 프로세스 하나가 끝난 뒤 두 마운트 폴더(`/bot`·`/work`)를 훑은 결과.
#[derive(Clone, Debug, Default)]
pub struct Swept {
    /// 세션 토큰을 이름이나 내용에 담은 파일
    pub hits: Vec<PathBuf>,
    /// 훑은 파일 수(CLI가 쓴 파일까지 훑었는지 보려고)
    pub files: usize,
    /// 훑은 토큰 길이(빈 값을 훑지 않았는지 보려고)
    pub token_len: usize,
}

/// `roots` 아래 모든 파일에서 `needle`을 이름이나 내용에 담은 것과 훑은 파일 수(심볼릭 링크는 따라가지 않는다).
pub fn sweep_files(roots: &[PathBuf], needle: &str) -> (Vec<PathBuf>, usize) {
    fn walk(dir: &Path, needle: &str, hits: &mut Vec<PathBuf>, files: &mut usize) {
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
                walk(&path, needle, hits, files);
            } else if kind.is_file() {
                *files += 1;
                if std::fs::read(&path)
                    .is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
                {
                    hits.push(path);
                }
            }
        }
    }
    assert!(!needle.is_empty(), "빈 값은 훑을 수 없다");
    let (mut hits, mut files) = (Vec::new(), 0);
    for root in roots {
        walk(root, needle, &mut hits, &mut files);
    }
    (hits, files)
}

/// 프로세스가 끝난 뒤 훑을 것: (폴더들, 찾을 토큰, 결과를 남길 곳)
pub struct Sweep {
    roots: Vec<PathBuf>,
    token: String,
    sink: Arc<Mutex<Vec<Swept>>>,
}

impl Sweep {
    /// 이 명세의 두 마운트 폴더(`/bot`·`/work`의 호스트 쪽)와 환경의 세션 토큰(`RELAY_TOKEN`). 토큰이 없으면 훑지 않는다.
    pub fn of(spec: &ModelSpec, sink: Arc<Mutex<Vec<Swept>>>) -> Option<Self> {
        let token = spec.env.iter().find(|(key, _)| key == "RELAY_TOKEN")?;
        Some(Self {
            roots: spec
                .readonly_mounts
                .iter()
                .chain(spec.scratch.iter())
                .map(|mount| mount.host.clone())
                .collect(),
            token: token.1.clone(),
            sink,
        })
    }

    pub fn run(self) {
        let (hits, files) = sweep_files(&self.roots, &self.token);
        if let Ok(mut sink) = self.sink.lock() {
            sink.push(Swept {
                hits,
                files,
                token_len: self.token.len(),
            });
        }
    }
}

/// 어떤 모델 샌드박스든 감싸서, 프로세스가 끝난 직후(호출 폴더가 지워지기 전) 두 마운트 폴더를 훑는다(실제 VM 시험).
pub struct Sweeping<S> {
    inner: S,
    swept: Arc<Mutex<Vec<Swept>>>,
}

impl<S> Sweeping<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            swept: Arc::default(),
        }
    }

    /// 끝난 프로세스마다의 훑기 결과(시작 순서)
    pub fn swept(&self) -> Vec<Swept> {
        self.swept.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }
}

struct SweepingProcess {
    inner: Box<dyn ModelProcess>,
    sweep: Option<Sweep>,
}

impl ModelProcess for SweepingProcess {
    fn next_line(&mut self) -> LineFuture<'_> {
        self.inner.next_line()
    }
    fn kill(&mut self) -> UnitFuture<'_> {
        self.inner.kill()
    }
    fn wait(&mut self) -> ExitFuture<'_> {
        Box::pin(async move {
            let exit = self.inner.wait().await;
            if let Some(sweep) = self.sweep.take() {
                sweep.run();
            }
            exit
        })
    }
}

impl<S> Sandbox<ModelSpec> for Sweeping<S>
where
    S: Sandbox<ModelSpec, Process = Box<dyn ModelProcess>>,
{
    type Process = Box<dyn ModelProcess>;

    fn spawn(&self, spec: ModelSpec) -> Reply<'_, Self::Process> {
        let sweep = Sweep::of(&spec, self.swept.clone());
        let started = self.inner.spawn(spec);
        Box::pin(async move {
            let inner = started.await?;
            Ok(Box::new(SweepingProcess { inner, sweep }) as Box<dyn ModelProcess>)
        })
    }

    fn reap(&self, owner: &str) -> Reply<'_, ()> {
        self.inner.reap(owner)
    }
}
