//! In-memory `MemoryStore` test double enforcing owner-only private preview and hash-checked sharing.

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
    /// Mutation applied right after the next `preview` to simulate concurrent edits.
    edit_after_preview: Mutex<Option<(MemoryId, String)>>,
    /// Hook invoked right before the next `preview` reads state.
    before_preview: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

use super::lock;

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

    pub fn edit(&self, id: &str, text: &str) {
        lock(&self.entries)
            .get_mut(&MemoryId(id.into()))
            .expect("memory")
            .text = text.into();
    }

    pub fn before_next_preview(&self, hook: impl FnOnce() + Send + 'static) {
        *lock(&self.before_preview) = Some(Box::new(hook));
    }

    pub fn remove(&self, id: &str) {
        lock(&self.entries).remove(&MemoryId(id.into()));
    }

    /// Schedules a text edit immediately after the next `preview` returns the old text.
    pub fn edit_after_next_preview(&self, id: &str, text: &str) {
        *lock(&self.edit_after_preview) = Some((MemoryId(id.into()), text.into()));
    }

    pub fn visibility(&self, id: &str) -> Visibility {
        lock(&self.entries)[&MemoryId(id.into())].visibility
    }

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
            // Retrying a completed share returns the same ID without mutating visibility again.
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
