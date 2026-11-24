//! 같은 호출 위치(프로필·run·호출 ID)의 `tools/call`을 한 번에 하나씩만 처리한다.
//!
//! 확정 거절은 «표식을 읽고 → 브로커를 부르고 → 거절이면 표식을 남긴다»는 한 덩어리다. 이 덩어리가 겹치면 같은 위치를 한쪽은 표식으로 거절하고 다른 쪽은
//! 호출 기록을 만들어 실행하는 일이 생긴다. 위치마다 문을 하나 두어 동시 재전달이 서로를 기다리게 한다: 먼저 끝난 쪽의 표식이나 호출 기록을 나중 쪽이
//! 읽고 같은 결과를 쓴다(다른 위치는 서로 막지 않는다). 한 시도(claim) 안의 요청은 한 프로세스의 프런트로 들어오므로 프로세스 안의 문으로 충분하고,
//! 다른 시도는 현재 claim을 쥔 쪽만 표식과 호출을 기록할 수 있다는 저장소의 펜스가 막는다.

use bot_core::id::{CallId, ProfileId, RunId};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{Mutex as Door, OwnedMutexGuard};

pub(super) type Key = (ProfileId, RunId, CallId);

/// 열려 있는 위치의 문. 쓰는 쪽이 없어지면 목록에서 지워서 자라지 않는다.
#[derive(Default)]
pub(super) struct Slots {
    open: Mutex<HashMap<Key, Arc<Door<()>>>>,
}

/// 이 위치의 문을 쥔 동안 값이다. 놓으면 다음 요청이 들어온다(기다리다 취소돼도 정리된다).
pub(super) struct Entered<'a> {
    slots: &'a Slots,
    key: Key,
    door: Option<Arc<Door<()>>>,
    guard: Option<OwnedMutexGuard<()>>,
}

impl Slots {
    /// 이 위치의 문을 얻을 때까지 기다린다.
    pub(super) async fn enter(&self, key: Key) -> Entered<'_> {
        let door = self
            .open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key.clone())
            .or_default()
            .clone();
        // 기다리는 동안 이 future가 버려져도 `Entered`의 정리가 돈다(목록에 빈 문이 남지 않는다)
        let mut entered = Entered {
            slots: self,
            key,
            door: Some(door.clone()),
            guard: None,
        };
        entered.guard = Some(door.lock_owned().await);
        entered
    }

    /// 열려 있는 위치 수(시험용).
    #[cfg(test)]
    pub(super) fn open(&self) -> usize {
        self.open.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 이 위치의 문 앞에서 기다리는 요청 수(시험용). 문의 참조는 목록 하나와 문을 쥔 쪽 둘(`Entered`의 것과 잠금 수호자의 것)이고, 기다리는 쪽마다 둘이 더 있다.
    #[cfg(test)]
    pub(super) fn waiting(&self, key: &Key) -> usize {
        self.open
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .map_or(0, |door| Arc::strong_count(door).saturating_sub(3) / 2)
    }
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        // 문을 놓고 내 몫의 참조를 버린 뒤, 목록만 쥐고 있으면(기다리는 쪽이 없으면) 지운다. 새로 오는 쪽은 목록 잠금 안에서 참조를 얻으므로 어긋나지 않는다.
        self.guard.take();
        self.door.take();
        let mut open = self.slots.open.lock().unwrap_or_else(|e| e.into_inner());
        if open
            .get(&self.key)
            .is_some_and(|door| Arc::strong_count(door) == 1)
        {
            open.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::{sync::mpsc, time::timeout};

    fn key(call: &str) -> Key {
        (
            ProfileId("personal".into()),
            RunId("r1".into()),
            CallId(call.into()),
        )
    }

    #[tokio::test]
    async fn the_same_position_goes_one_at_a_time_and_other_positions_do_not_wait() {
        let slots = Arc::new(Slots::default());
        let first = slots.enter(key("cli-1-0")).await;
        // 다른 위치는 기다리지 않는다
        drop(slots.enter(key("cli-1-1")).await);
        // 같은 위치는 첫 쪽이 놓을 때까지 들어오지 못한다
        let (entered, mut seen) = mpsc::unbounded_channel();
        let second = {
            let slots = slots.clone();
            tokio::spawn(async move {
                let _in = slots.enter(key("cli-1-0")).await;
                let _ = entered.send(());
            })
        };
        assert!(
            timeout(Duration::from_millis(80), seen.recv())
                .await
                .is_err(),
            "문을 쥔 동안 같은 위치의 다음 요청은 들어오지 못한다"
        );
        drop(first);
        timeout(Duration::from_secs(5), seen.recv())
            .await
            .expect("놓으면 다음 요청이 들어온다")
            .expect("신호");
        second.await.unwrap();
        assert_eq!(slots.open(), 0, "쓰는 쪽이 없으면 목록에서 지운다");
    }

    #[tokio::test]
    async fn a_waiter_that_gives_up_leaves_nothing_behind() {
        let slots = Arc::new(Slots::default());
        let first = slots.enter(key("cli-2-0")).await;
        let giving_up = timeout(Duration::from_millis(40), slots.enter(key("cli-2-0"))).await;
        assert!(giving_up.is_err(), "기다리다 버렸다");
        assert_eq!(slots.open(), 1, "문을 쥔 쪽 하나만 남는다");
        drop(first);
        assert_eq!(slots.open(), 0);
        // 버린 뒤에도 같은 위치는 다시 쓸 수 있다
        drop(slots.enter(key("cli-2-0")).await);
        assert_eq!(slots.open(), 0);
    }
}
