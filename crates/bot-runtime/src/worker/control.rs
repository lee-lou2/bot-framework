//! 실행 중 run의 heartbeat와 중단 신호. 리스를 계속 늘리고, 취소 요청을 실행기에 알리고, 리스를 잃으면 멈추게 한다.
//!
//! - `renew`가 `cancel_requested`를 돌려주면 중단 신호를 올린다(취소). run은 `fail(Permanent("cancelled"))`로 끝난다.
//! - `Stale`(옛 세대·만료·승인 대기로 저장소가 멈춤)이거나 저장소가 연속으로 `heartbeat_misses`번 응답하지 못하면 리스를 잃은 것이다.
//!   중단 신호를 올리고 이후 어떤 결과도 쓰지 않는다(다른 워커가 이어받는다).
//! - 종료 반납 신호(`release`)가 오면 같은 중단 신호를 올리되 `shutdown()`만 켠다. 하트비트는 정리·반납이 끝날 때까지 계속한다(리스가 살아 있어야 반납이 받아들여진다).

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
                // 송신 쪽이 없으면 반납 신호가 올 일이 없다(시험·단발 실행)
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

    /// 실행기에 줄 취소 수신단(`true`가 되면 멈춘다).
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

    /// 종료 반납 신호 수신단. 취소·리스 상실과 달리 이미 시작한 도구 호출도 이 신호로는 끊는다(정리가 프로세스 종료를 확인한다).
    pub(crate) fn released(&self) -> watch::Receiver<bool> {
        self.released.clone()
    }

    /// 종료 반납 신호로 멈췄는가(취소·리스 상실이 아니다).
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
