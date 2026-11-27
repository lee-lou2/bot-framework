//! Mount directory scanner that checks `/bot` and `/work` for leaked session tokens right after a model process exits.
#![allow(dead_code)]

use bot_core::{
    port::Reply,
    sandbox::{ExitFuture, LineFuture, ModelProcess, ModelSpec, Sandbox, UnitFuture},
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Default)]
pub struct Swept {
    pub hits: Vec<PathBuf>,
    pub files: usize,
    pub token_len: usize,
}

/// Scans all files under `roots` (without following symlinks) for `needle` in paths or contents.
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
    assert!(!needle.is_empty(), "needle must not be empty");
    let (mut hits, mut files) = (Vec::new(), 0);
    for root in roots {
        walk(root, needle, &mut hits, &mut files);
    }
    (hits, files)
}

pub struct Sweep {
    roots: Vec<PathBuf>,
    token: String,
    sink: Arc<Mutex<Vec<Swept>>>,
}

impl Sweep {
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

/// Wraps any `ModelSandbox` to sweep its mounted host directories immediately after `wait()` completes.
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
