//! `MemoryStore` 가짜(메모리). 실제 저장소가 지켜야 할 규칙을 그대로 적었다.
//! - `preview`는 ID 한 건만 읽고(스캔 없음), 소유자 본인의 DM·CLI에서만 private 본문을 준다. private를 읽으면 요청 트리 플래그를 올린다.
//! - `share`는 지금 본문으로 지문을 다시 계산해 호출에 묶인 지문과 같을 때만 승인 소비·가시성 변경·호출 완료를 한 번에 한다.
//!   본문이 바뀌었으면 `Conflict`이고 승인은 소비되지 않는다.

use super::MemLedger;
use crate::canon::sha256;
use bot_core::{
    command::{MemoryChange, MemoryHit, MemoryPreview, MemorySeed, memory_share_hash},
    id::*,
    ledger::{CallLedger, StartCall},
    port::{Error, MemoryStore, Reply},
    state::{ConversationKind, Visibility},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

struct Entry {
    owner: PrincipalId,
    visibility: Visibility,
    text: String,
}

pub struct MemMemory {
    ledger: Arc<MemLedger>,
    entries: Mutex<HashMap<MemoryId, Entry>>,
    previews: Mutex<Vec<MemoryId>>,
    shared: Mutex<Vec<MemoryId>>,
    /// 다음 `preview`가 끝난 바로 뒤에 적용할 편집(읽은 뒤 공유 전에 본문이 바뀌는 경쟁).
    edit_after_preview: Mutex<Option<(MemoryId, String)>>,
    /// 다음 `preview`가 읽기 전에 한 번 실행할 일(다른 프로세스가 그 사이에 끼어드는 경쟁).
    before_preview: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl MemMemory {
    pub fn new(ledger: Arc<MemLedger>) -> Self {
        Self {
            ledger,
            entries: Mutex::default(),
            previews: Mutex::default(),
            shared: Mutex::default(),
            edit_after_preview: Mutex::default(),
            before_preview: Mutex::default(),
        }
    }

    pub fn add(&self, id: &str, owner: &str, visibility: Visibility, text: &str) {
        lock(&self.entries).insert(
            MemoryId(id.into()),
            Entry {
                owner: PrincipalId(owner.into()),
                visibility,
                text: text.into(),
            },
        );
    }

    /// 소유자가 카드를 본 뒤 본문을 고친 것처럼 바꾼다.
    pub fn edit(&self, id: &str, text: &str) {
        lock(&self.entries)
            .get_mut(&MemoryId(id.into()))
            .expect("memory")
            .text = text.into();
    }

    /// 다음 `preview`가 읽기 전에 `hook`을 한 번 실행한다.
    pub fn before_next_preview(&self, hook: impl FnOnce() + Send + 'static) {
        *lock(&self.before_preview) = Some(Box::new(hook));
    }

    /// 기억을 지운다.
    pub fn remove(&self, id: &str) {
        lock(&self.entries).remove(&MemoryId(id.into()));
    }

    /// 다음 `preview`가 본문을 읽어 간 직후 본문을 바꾼다(그 읽기에는 옛 본문이 나간다).
    pub fn edit_after_next_preview(&self, id: &str, text: &str) {
        *lock(&self.edit_after_preview) = Some((MemoryId(id.into()), text.into()));
    }

    pub fn visibility(&self, id: &str) -> Visibility {
        lock(&self.entries)[&MemoryId(id.into())].visibility
    }

    /// `preview`가 읽은 ID들(한 건씩 읽었는지 본다).
    pub fn previews(&self) -> Vec<MemoryId> {
        lock(&self.previews).clone()
    }

    pub fn shared(&self) -> Vec<MemoryId> {
        lock(&self.shared).clone()
    }
}

impl MemoryStore for MemMemory {
    fn search(&self, _: Attempt, _: String) -> Reply<'_, Vec<MemoryHit>> {
        Box::pin(async { Err(Error::Unavailable) })
    }

    fn preview(&self, attempt: Attempt, id: MemoryId) -> Reply<'_, MemoryPreview> {
        Box::pin(async move {
            let ctx = self.ledger.context(attempt).await?;
            let hook = lock(&self.before_preview).take();
            if let Some(hook) = hook {
                hook();
            }
            let entries = lock(&self.entries);
            let entry = entries.get(&id).ok_or(Error::Missing)?;
            let owner_channel = matches!(
                ctx.conversation.kind,
                ConversationKind::Direct | ConversationKind::Cli
            );
            if entry.owner != ctx.principal
                || (entry.visibility == Visibility::Private && !owner_channel)
            {
                return Err(Error::Denied);
            }
            if entry.visibility == Visibility::Private {
                self.ledger.set_private_recall(&ctx.root.0);
            }
            lock(&self.previews).push(id.clone());
            let preview = MemoryPreview {
                id,
                visibility: entry.visibility,
                text: entry.text.clone(),
                sha256: sha256(&entry.text),
            };
            drop(entries);
            if let Some((target, text)) = lock(&self.edit_after_preview).take()
                && let Some(entry) = lock(&self.entries).get_mut(&target)
            {
                entry.text = text;
            }
            Ok(preview)
        })
    }

    fn change(&self, _: Attempt, _: MemoryChange) -> Reply<'_, MemoryId> {
        Box::pin(async { Err(Error::Unavailable) })
    }

    fn share(&self, start: StartCall, id: MemoryId) -> Reply<'_, MemoryId> {
        Box::pin(async move {
            // 완료한 호출의 재시도는 가시성을 다시 바꾸지 않고 같은 기억이면 그대로 돌려준다(저장소와 같다).
            if self.ledger.call_done(&start.attempt.run, &start.call) {
                return Ok(id);
            }
            let ctx = self.ledger.context(start.attempt.clone()).await?;
            let expected = {
                let entries = lock(&self.entries);
                let entry = entries.get(&id).ok_or(Error::Missing)?;
                if entry.owner != ctx.principal || entry.visibility != Visibility::Private {
                    return Err(Error::Conflict);
                }
                memory_share_hash(&id, &sha256(&entry.text))
            };
            self.ledger.complete_share(start, expected, &id.0)?;
            lock(&self.entries).get_mut(&id).expect("memory").visibility = Visibility::Profile;
            lock(&self.shared).push(id.clone());
            Ok(id)
        })
    }

    fn seed(&self, _: MemorySeed) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

#[allow(dead_code)]
fn _uses_call_ledger(_: &dyn CallLedger) {}
