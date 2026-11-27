//! Redacting `SecretStore` wrapper and periodic background re-read loop for secret rotation.
use crate::{host::HostFile, validate::all_needed_secrets};
use bot_admin::trace::Redactor;
use bot_core::{
    id::{ProfileId, SecretName},
    port::{ProfileSecrets, Reply, SecretStore, SecretValue},
    profile::ProfileCatalog,
};
use bot_profile::Registry;
use std::{sync::Arc, time::Duration};
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

pub struct Watched {
    inner: Arc<dyn SecretStore>,
    redactor: Redactor,
}

impl Watched {
    /// Only constructed by `trace::start` (enforced by `serve/watched/tests.rs`) so raw stores cannot reach roles.
    pub(super) fn new(inner: Arc<dyn SecretStore>, redactor: Redactor) -> Self {
        Self { inner, redactor }
    }

    pub fn register(&self, secret: &str) {
        add_forms(&self.redactor, secret);
    }

    #[cfg(test)]
    pub(super) fn plain(inner: Arc<dyn SecretStore>) -> Arc<Self> {
        Arc::new(Self::new(inner, Redactor::default()))
    }
}

impl SecretStore for Watched {
    fn for_profile(&self, profile: ProfileId) -> Reply<'_, Box<dyn ProfileSecrets>> {
        Box::pin(async move {
            let inner = self.inner.for_profile(profile).await?;
            Ok(Box::new(WatchedProfile {
                inner,
                redactor: self.redactor.clone(),
            }) as Box<dyn ProfileSecrets>)
        })
    }
}

struct WatchedProfile {
    inner: Box<dyn ProfileSecrets>,
    redactor: Redactor,
}

impl ProfileSecrets for WatchedProfile {
    fn resolve(&self, name: SecretName) -> Reply<'_, SecretValue> {
        Box::pin(async move {
            let value = self.inner.resolve(name).await?;
            // Register in the redactor before returning the secret to the caller.
            if let Ok(text) = std::str::from_utf8(&value.0) {
                add_forms(&self.redactor, text);
            }
            Ok(value)
        })
    }
}

/// Registers both raw and trimmed forms so trailing newlines do not defeat redaction, while ignoring whitespace-only values.
fn add_forms(redactor: &Redactor, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    redactor.add(text);
    redactor.add(trimmed);
}

pub(super) type Plan = Vec<(ProfileId, Vec<SecretName>)>;

pub(super) fn declared(host: &HostFile, registry: &Registry) -> Plan {
    all_needed_secrets(registry.active().values().map(AsRef::as_ref), host)
        .into_iter()
        .map(|(profile, names)| {
            let parsed = names
                .iter()
                .filter_map(|name| SecretName::parse(name))
                .collect::<Vec<_>>();
            (profile, parsed)
        })
        .filter(|(_, names)| !names.is_empty())
        .collect()
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Pace {
    pub every: Duration,
    pub read: Duration,
    pub gap: Duration,
}

impl Pace {
    pub(super) fn new(every: Duration) -> Self {
        Self {
            every,
            read: Duration::from_secs(5),
            gap: Duration::from_millis(25),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Round {
    pub read: usize,
    pub failed: usize,
    pub timed_out: usize,
}

/// Reads each declared secret once to populate the redactor, dropping values immediately.
pub(super) async fn read_round(secrets: &Watched, plan: &Plan, pace: Pace) -> Round {
    let mut round = Round::default();
    for (profile, names) in plan {
        let handle = match timeout(pace.read, secrets.for_profile(profile.clone())).await {
            Ok(Ok(handle)) => handle,
            Ok(Err(_)) => {
                round.failed += names.len();
                continue;
            }
            Err(_) => {
                round.timed_out += names.len();
                continue;
            }
        };
        for name in names {
            match timeout(pace.read, handle.resolve(name.clone())).await {
                Ok(Ok(_)) => round.read += 1,
                Ok(Err(_)) => round.failed += 1,
                Err(_) => round.timed_out += 1,
            }
            if !pace.gap.is_zero() {
                sleep(pace.gap).await;
            }
        }
    }
    round
}

/// Backs off exponentially (up to 10x) after rounds with timeouts so locked keychains are not hammered.
pub(super) fn next_wait(every: Duration, current: Duration, round: &Round) -> Duration {
    if round.timed_out > 0 {
        current.saturating_mul(2).min(every.saturating_mul(10))
    } else {
        every
    }
}

pub(super) async fn reread(secrets: Arc<Watched>, plan: Plan, pace: Pace, stop: CancellationToken) {
    if plan.is_empty() {
        return;
    }
    let mut wait = pace.every;
    loop {
        tokio::select! {
            () = stop.cancelled() => return,
            () = sleep(wait) => {}
        }
        let round = tokio::select! {
            () = stop.cancelled() => return,
            round = read_round(&secrets, &plan, pace) => round,
        };
        wait = next_wait(pace.every, wait, &round);
        // Log counts only; never emit secret names or values.
        tracing::debug!(
            read = round.read,
            failed = round.failed,
            timed_out = round.timed_out,
            "reread declared secrets"
        );
    }
}

#[cfg(test)]
mod tests;
