//! In-memory `MemoryStore` test double enforcing owner-only private preview and hash-checked sharing.

use super::MemLedger;
use crate::canon::sha256;
use bot_core::{
    command::{MemoryChange, MemoryHit, MemoryPreview, MemorySeed, memory_share_hash},
    id::*,
    ledger::{CallLedger, StartCall},
    memory::{MemoryQuery, MemorySave, MemorySaved, forget_seal, save_seal},
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

    pub fn contains(&self, id: &str) -> bool {
        lock(&self.entries).contains_key(&MemoryId(id.into()))
    }

    pub fn len(&self) -> usize {
        lock(&self.entries).len()
    }

    pub fn is_empty(&self) -> bool {
        lock(&self.entries).is_empty()
    }
}

impl MemoryStore for MemMemory {
    fn search(&self, attempt: Attempt, query: String) -> Reply<'_, Vec<MemoryHit>> {
        self.search_hybrid(attempt, MemoryQuery::lexical(query))
    }

    fn search_hybrid(&self, attempt: Attempt, query: MemoryQuery) -> Reply<'_, Vec<MemoryHit>> {
        Box::pin(async move {
            let ctx = self.ledger.context(attempt).await?;
            let owner_channel = matches!(
                ctx.conversation.kind,
                ConversationKind::Direct | ConversationKind::Cli
            );
            let q = query.text.to_lowercase();
            let entries = lock(&self.entries);
            let mut hits: Vec<MemoryHit> = entries
                .iter()
                .filter(|(_, e)| {
                    e.visibility == Visibility::Profile
                        || (owner_channel && e.owner == ctx.principal)
                })
                .filter(|(_, e)| q.is_empty() || e.text.to_lowercase().contains(&q))
                .map(|(id, e)| MemoryHit {
                    id: id.clone(),
                    visibility: e.visibility,
                    text: e.text.clone(),
                })
                .collect();
            hits.sort_by(|a, b| a.id.0.cmp(&b.id.0));
            hits.truncate(query.limit as usize);
            if hits.iter().any(|h| h.visibility == Visibility::Private) {
                self.ledger.set_private_recall(&ctx.root.0);
            }
            Ok(hits)
        })
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

    fn save(&self, start: StartCall, spec: MemorySave) -> Reply<'_, MemorySaved> {
        Box::pin(async move {
            let (_, expected) =
                save_seal(&spec.text, spec.kind, spec.supersedes.as_ref()).ok_or(Error::Invalid)?;
            let ctx = self.ledger.context(start.attempt.clone()).await?;
            if !matches!(
                ctx.conversation.kind,
                ConversationKind::Direct | ConversationKind::Cli
            ) {
                return Err(Error::Denied);
            }
            let (id, deduplicated) = {
                let mut entries = lock(&self.entries);
                if let Some(old) = &spec.supersedes {
                    let existing = entries.get(old).ok_or(Error::Missing)?;
                    if existing.owner != ctx.principal {
                        return Err(Error::Denied);
                    }
                }
                if spec.supersedes.is_none()
                    && let Some((existing_id, _)) = entries.iter().find(|(_, e)| {
                        e.owner == ctx.principal
                            && e.visibility == Visibility::Private
                            && e.text.trim() == spec.text.trim()
                    })
                {
                    (existing_id.clone(), true)
                } else {
                    let id = MemoryId(format!("m-saved-{}", entries.len() + 1));
                    if let Some(old) = &spec.supersedes {
                        entries.remove(old);
                    }
                    entries.insert(
                        id.clone(),
                        Entry {
                            owner: ctx.principal,
                            visibility: Visibility::Private,
                            text: spec.text,
                        },
                    );
                    (id, false)
                }
            };
            let payload = format!("{{\"deduplicated\":{deduplicated},\"id\":\"{}\"}}", id.0);
            self.ledger
                .complete_memory_op(start, expected, payload, "memory.saved")?;
            Ok(MemorySaved { id, deduplicated })
        })
    }

    fn forget(&self, start: StartCall, id: MemoryId) -> Reply<'_, MemoryId> {
        Box::pin(async move {
            let (_, expected) = forget_seal(&id).ok_or(Error::Invalid)?;
            let ctx = self.ledger.context(start.attempt.clone()).await?;
            if !matches!(
                ctx.conversation.kind,
                ConversationKind::Direct | ConversationKind::Cli
            ) {
                return Err(Error::Denied);
            }
            {
                let entries = lock(&self.entries);
                let entry = entries.get(&id).ok_or(Error::Missing)?;
                if entry.owner != ctx.principal {
                    return Err(Error::Denied);
                }
            }
            let payload = format!("{{\"id\":\"{}\"}}", id.0);
            self.ledger
                .complete_memory_op(start, expected, payload, "memory.forgotten")?;
            lock(&self.entries).remove(&id);
            Ok(id)
        })
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
