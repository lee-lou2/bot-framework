//! `step` 읽기에 끼어드는 시험 갈고리: 지정한 키 접두사의 **다음** 읽기를 붙잡아 두거나, 그 읽기가 값을 돌려주는 바로 그 순간에 일을 시킨다
//! (시간 추정 없이 «읽는 도중»·«읽기가 끝나는 순간»에 회수 같은 사건을 끼워 넣으려는 시험이 쓴다).

use super::Hook;
use bot_core::{
    command::JsonPayload,
    port::{Error, Reply},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};

#[derive(Default)]
pub(super) struct StepProbe {
    hold: Mutex<Option<(String, Arc<Semaphore>)>>,
    after: Mutex<Option<(String, Hook)>>,
    /// 붙잡힌 읽기가 들어설 때마다 알린다.
    held: Notify,
}

/// 이번 읽기가 받은 갈고리(없으면 아무것도 하지 않는다).
#[derive(Default)]
pub(super) struct Taken {
    hold: Option<Arc<Semaphore>>,
    after: Option<Hook>,
}

use super::super::lock;

/// 키가 접두사로 시작할 때만 갈고리를 꺼낸다(한 번 쓰면 없어진다).
fn take_if<T>(slot: &Mutex<Option<(String, T)>>, key: &str) -> Option<T> {
    let mut slot = lock(slot);
    match slot.take() {
        Some((prefix, value)) if key.starts_with(&prefix) => Some(value),
        other => {
            *slot = other;
            None
        }
    }
}

impl StepProbe {
    pub(super) fn hold(&self, prefix: &str) -> Arc<Semaphore> {
        let gate = Arc::new(Semaphore::new(0));
        *lock(&self.hold) = Some((prefix.to_owned(), gate.clone()));
        gate
    }

    pub(super) fn after(&self, prefix: &str, hook: Hook) {
        *lock(&self.after) = Some((prefix.to_owned(), hook));
    }

    pub(super) async fn entered(&self) {
        self.held.notified().await;
    }

    pub(super) fn take(&self, key: &str) -> Taken {
        Taken {
            hold: take_if(&self.hold, key),
            after: take_if(&self.after, key),
        }
    }

    /// 읽기 응답을 갈고리에 맞춰 감싼다: 붙잡히면 문이 열릴 때까지 기다리고(들어섰다고 알린다), `delay`만큼 기다린 뒤 값을 돌려주는 순간 `after`를 한다.
    pub(super) fn wrap<'a>(
        &'a self,
        taken: Taken,
        delay: Duration,
        reply: Reply<'a, Option<JsonPayload>>,
    ) -> Reply<'a, Option<JsonPayload>> {
        if delay.is_zero() && taken.hold.is_none() && taken.after.is_none() {
            return reply;
        }
        Box::pin(async move {
            if let Some(gate) = taken.hold {
                self.held.notify_one();
                let _ = gate.acquire().await;
            }
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let out: Result<Option<JsonPayload>, Error> = reply.await;
            if let Some(after) = taken.after {
                after();
            }
            out
        })
    }
}
