//! Shutdown quiesce confirmation (`bot_runtime::Quiesce`) ensuring VM and tool processes exit before release.
// Order: reap model VM -> reap started tool calls -> revoke session tokens -> re-scan for late calls.
// Returning an error leaves the run leased so normal lease-takeover recovery reaps any stragglers.

use bot_broker::{front::SessionTable, ports::ToolRunner};
use bot_core::{
    id::Attempt,
    port::{Error, Reply},
    sandbox::ModelSandbox,
};
use bot_runtime::{Quiesce, model::container::ContainerSandbox};
use bot_store::Store;
use std::{collections::BTreeSet, sync::Arc};

const PASSES: usize = 3;

pub trait StartedCalls: Send + Sync {
    fn owners(&self, attempt: Attempt) -> Reply<'_, Vec<String>>;
}

impl StartedCalls for Store {
    fn owners(&self, attempt: Attempt) -> Reply<'_, Vec<String>> {
        Box::pin(async move { self.started_owners(&attempt).await })
    }
}

pub struct Quiescer {
    started: Arc<dyn StartedCalls>,
    tools: Arc<dyn ToolRunner>,
    model: Option<(Arc<SessionTable>, Arc<ModelSandbox>)>,
}

impl Quiescer {
    pub fn new(
        started: Arc<dyn StartedCalls>,
        tools: Arc<dyn ToolRunner>,
        model: Option<(Arc<SessionTable>, Arc<ModelSandbox>)>,
    ) -> Self {
        Self {
            started,
            tools,
            model,
        }
    }

    async fn reap_started(
        &self,
        attempt: &Attempt,
        done: &mut BTreeSet<String>,
    ) -> Result<usize, Error> {
        let owners = self.started.owners(attempt.clone()).await?;
        let fresh: Vec<String> = owners
            .into_iter()
            .filter(|owner| !done.contains(owner))
            .collect();
        for owner in &fresh {
            self.tools.reap(owner.clone()).await?;
            done.insert(owner.clone());
        }
        Ok(fresh.len())
    }
}

impl Quiesce for Quiescer {
    fn quiesce(&self, attempt: Attempt) -> Reply<'_, ()> {
        Box::pin(async move {
            if let Some((_, sandbox)) = &self.model {
                sandbox.reap(&ContainerSandbox::owner_of(&attempt)).await?;
            }
            let mut done = BTreeSet::new();
            self.reap_started(&attempt, &mut done).await?;
            if let Some((sessions, _)) = &self.model {
                sessions.revoke_attempt(&attempt);
            }
            // Catch calls started concurrently from requests already in flight before revocation.
            for _ in 0..PASSES {
                if self.reap_started(&attempt, &mut done).await? == 0 {
                    return Ok(());
                }
            }
            Err(Error::Unavailable)
        })
    }
}

#[cfg(test)]
mod tests;
