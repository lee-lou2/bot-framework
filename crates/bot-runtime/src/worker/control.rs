//! Per-run lease heartbeat and stop/cancel/shutdown-release coordination.

use bot_core::{
    id::Attempt,
    port::{Error, RunStore},
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle};

pub(crate) struct Control {
    stop: watch::Receiver<bool>,
    lost: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    released: watch::Receiver<bool>,
    tasks: [JoinHandle<()>; 2],
}

impl Control {
    pub(crate) fn start(
        runs: Arc<dyn RunStore>,
        attempt: Attempt,
        every: Duration,
        misses: u32,
        release: watch::Receiver<bool>,
    ) -> Self {
        let released = release.clone();
        let mut release = release;
        let (tx, stop) = watch::channel(false);
        let tx = Arc::new(tx);
        let lost = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (lost_flag, cancel_flag, beat_tx) = (lost.clone(), cancelled.clone(), tx.clone());
        // Heartbeat keeps running during shutdown release so the lease stays valid until `release_for_shutdown` commits.
        let beat = tokio::spawn(async move {
            let tx = beat_tx;
            let mut missed = 0u32;
            loop {
                tokio::time::sleep(every).await;
                match runs.renew(attempt.clone()).await {
                    Ok(beat) => {
                        missed = 0;
                        if beat.cancel_requested {
                            cancel_flag.store(true, Ordering::SeqCst);
                            let _ = tx.send(true);
                        }
                    }
                    Err(Error::Stale | Error::Denied | Error::Missing) => {
                        lost_flag.store(true, Ordering::SeqCst);
                        let _ = tx.send(true);
                        return;
                    }
                    Err(_) => {
                        missed += 1;
                        if missed >= misses.max(1) {
                            lost_flag.store(true, Ordering::SeqCst);
                            let _ = tx.send(true);
                            return;
                        }
                    }
                }
            }
        });
        let flag = shutdown.clone();
        let release_tx = tx;
        let watcher = tokio::spawn(async move {
            loop {
                if *release.borrow_and_update() {
                    flag.store(true, Ordering::SeqCst);
                    let _ = release_tx.send(true);
                    return;
                }
                if release.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        });
        Self {
            stop,
            lost,
            cancelled,
            shutdown,
            released,
            tasks: [beat, watcher],
        }
    }

    pub(crate) fn signal(&self) -> watch::Receiver<bool> {
        self.stop.clone()
    }

    pub(crate) fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    pub(crate) fn lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    pub(crate) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    pub(crate) fn released(&self) -> watch::Receiver<bool> {
        self.released.clone()
    }

    pub(crate) fn shutdown(&self) -> bool {
        self.shutdown.load(Ordering::SeqCst)
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
