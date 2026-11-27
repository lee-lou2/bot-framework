//! In-memory `ScheduleRoutes` test double resolving profile destination aliases to canonical routes.

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
    input: String,
    canonical: String,
}

type Hook = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub struct MemRoutes {
    entries: Mutex<Vec<Entry>>,
    asked: Mutex<usize>,
    /// Forces `resolve` to fail with `Error::Unavailable`.
    outage: Mutex<bool>,
    /// Hook fired immediately before the Nth `resolve` call.
    hook: Mutex<Option<(usize, Hook)>>,
}

use super::lock;

impl MemRoutes {
    pub fn new() -> Self {
        Self::default()
    }

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

    /// Removes all entries targeting `canonical`.
    pub fn unregister(&self, canonical: &str) {
        lock(&self.entries).retain(|entry| entry.canonical != canonical);
    }

    pub fn outage(&self, down: bool) {
        *lock(&self.outage) = down;
    }

    pub fn asked(&self) -> usize {
        *lock(&self.asked)
    }

    /// Fires `action` once right before the cumulative Nth `resolve` call.
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
