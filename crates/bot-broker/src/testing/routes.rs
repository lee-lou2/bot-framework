//! `ScheduleRoutes` 가짜(메모리): 같은 프로필의 호스트 등록부. 모델이 쓰는 이름(입력 ID)을 정규 전송 경로(`팀ID/채널ID`)로 확정하고,
//! 이미 정규 경로인 ID는 지금도 등록돼 있을 때만 그대로 돌려준다. 바인딩·종류·스레드는 보존한다. 등록되지 않았거나 재지정돼 더는 그 경로가 아니면 `Denied`,
//! 빈 ID는 `Invalid`다. 외부 서비스는 묻지 않는다.

use bot_core::{
    id::{BindingId, ProfileId},
    port::Error,
    schedule::{DeliveryTarget, ScheduleRoutes},
    state::{Conversation, ConversationKind},
};
use std::sync::{Arc, Mutex};

struct Entry {
    profile: ProfileId,
    binding: String,
    kind: ConversationKind,
    /// 모델이 쓰는 이름.
    input: String,
    /// 정규 전송 경로.
    canonical: String,
}

type Hook = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub struct MemRoutes {
    entries: Mutex<Vec<Entry>>,
    asked: Mutex<usize>,
    /// 있으면 `resolve`가 등록부를 확인하지 못한다(`Unavailable`): 일시 오류.
    outage: Mutex<bool>,
    /// `resolve`가 이 횟수째로 불릴 때 확인 직전에 한 번 실행한다(확인과 확인 사이에 생기는 변화를 만든다).
    hook: Mutex<Option<(usize, Hook)>>,
}

use super::lock;

impl MemRoutes {
    pub fn new() -> Self {
        Self::default()
    }

    /// `input`(모델이 쓰는 이름)을 정규 경로 `canonical`로 등록한다.
    pub fn register(
        &self,
        profile: &str,
        binding: &str,
        kind: ConversationKind,
        input: &str,
        canonical: &str,
    ) {
        lock(&self.entries).push(Entry {
            profile: ProfileId(profile.into()),
            binding: binding.into(),
            kind,
            input: input.into(),
            canonical: canonical.into(),
        });
    }

    /// 이 정규 경로를 등록부에서 뺀다(그 경로를 가리키는 모든 이름과 함께).
    pub fn unregister(&self, canonical: &str) {
        lock(&self.entries).retain(|entry| entry.canonical != canonical);
    }

    /// 켜면 이후 `resolve`가 확인하지 못한다(일시 오류, 등록부는 그대로다). 끄면 다시 정상이다.
    pub fn outage(&self, down: bool) {
        *lock(&self.outage) = down;
    }

    /// 등록부에 물은 횟수.
    pub fn asked(&self) -> usize {
        *lock(&self.asked)
    }

    /// 등록부에 `n`번째로 물을 때(지금까지 물은 횟수와 별개로 누적 `n`번째) 확인하기 직전에 `action`을 한 번 한다.
    pub fn on_nth_ask(&self, n: usize, action: impl Fn() + Send + Sync + 'static) {
        *lock(&self.hook) = Some((n, Arc::new(action)));
    }
}

impl ScheduleRoutes for MemRoutes {
    fn resolve(
        &self,
        profile: &ProfileId,
        target: &DeliveryTarget,
    ) -> Result<DeliveryTarget, Error> {
        let count = {
            let mut asked = lock(&self.asked);
            *asked += 1;
            *asked
        };
        let action = {
            let mut hook = lock(&self.hook);
            match hook.as_ref() {
                Some((n, _)) if *n == count => hook.take().map(|(_, action)| action),
                _ => None,
            }
        };
        if let Some(action) = action {
            action();
        }
        if *lock(&self.outage) {
            return Err(Error::Unavailable);
        }
        let id = &target.conversation.id;
        if id.trim().is_empty() {
            return Err(Error::Invalid);
        }
        let entries = lock(&self.entries);
        let entry = entries
            .iter()
            .find(|entry| {
                entry.profile == *profile
                    && entry.binding == target.binding.0
                    && entry.kind == target.conversation.kind
                    && (entry.input == *id || entry.canonical == *id)
            })
            .ok_or(Error::Denied)?;
        Ok(DeliveryTarget {
            binding: BindingId(entry.binding.clone()),
            conversation: Conversation {
                kind: entry.kind.clone(),
                id: entry.canonical.clone(),
                thread: target.conversation.thread.clone(),
            },
        })
    }
}
