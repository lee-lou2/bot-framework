//! Per-connection ownership monitor combining session revocation and periodic ledger checks.
//!
//! Pauses output when ownership cannot be confirmed and terminates after `FrontConfig.verify_grace`
//! from the first unconfirmed timestamp. One shared deadline bounds both ledger queries and retry sleeps;
//! revocation or ledger rejection (`Stale`/`Denied`) ends immediately.

use super::{Session, Shared};
use bot_core::{
    id::Attempt,
    port::{Error, Reply},
    state::RunContext,
};
use std::time::Duration;
use tokio::{
    sync::watch,
    time::{Instant, sleep_until, timeout_at},
};

pub(super) enum Change {
    Confirmed,
    Unconfirmed,
    Lost,
    Expired,
}

pub(super) fn revoked(gone: &watch::Receiver<bool>) -> bool {
    *gone.borrow() || gone.has_changed().is_err()
}

pub(super) struct Ownership<'a> {
    shared: &'a Shared,
    session: &'a Session,
    gone: watch::Receiver<bool>,
    confirmed: bool,
    /// First unconfirmed instant (`None` while confirmed); `since + verify_grace` is the sole deadline.
    since: Option<Instant>,
    next_check: Instant,
    /// Preserved across `changed()` cancellations so rapid stream events never starve slow ledger checks.
    inflight: Option<(Reply<'a, RunContext>, Instant)>,
}

impl<'a> Ownership<'a> {
    pub(super) fn new(
        shared: &'a Shared,
        session: &'a Session,
        gone: watch::Receiver<bool>,
    ) -> Self {
        Self {
            shared,
            session,
            gone,
            confirmed: true,
            since: None,
            next_check: Instant::now() + shared.cfg.revoke_check,
            inflight: None,
        }
    }

    pub(super) fn attempt(&self) -> &Attempt {
        &self.session.attempt
    }

    fn deadline(&self) -> Option<Instant> {
        self.since.map(|at| at + self.shared.cfg.verify_grace)
    }

    fn pace(&self) -> Duration {
        let every = self.shared.cfg.revoke_check;
        if self.confirmed {
            return every;
        }
        (self.shared.cfg.verify_grace / 4).clamp(
            Duration::from_millis(10),
            every.max(Duration::from_millis(10)),
        )
    }

    fn confirm(&mut self) {
        self.confirmed = true;
        self.since = None;
        self.inflight = None;
        self.next_check = Instant::now() + self.shared.cfg.revoke_check;
    }

    fn lose_sight(&mut self, at: Instant) {
        self.confirmed = false;
        self.since.get_or_insert(at);
    }

    /// Re-verifies ownership immediately before emitting a response, retrying until `verify_grace`.
    pub(super) async fn settle(&mut self) -> Result<(), Error> {
        self.inflight = None;
        let since = self.since.unwrap_or_else(Instant::now);
        let deadline = since + self.shared.cfg.verify_grace;
        loop {
            if revoked(&self.gone) {
                return Err(Error::Stale);
            }
            if Instant::now() >= deadline {
                self.lose_sight(since);
                return Err(Error::Unavailable);
            }
            let check = timeout_at(
                deadline,
                self.shared.parts.model.verify(self.session.attempt.clone()),
            );
            let checked = tokio::select! {
                _ = self.gone.changed() => return Err(Error::Stale),
                checked = check => checked,
            };
            match checked {
                // Reject a verification that completed after the deadline even if both polled ready together.
                Ok(Ok(ctx)) if ctx.agent == self.session.agent && Instant::now() < deadline => {
                    self.confirm();
                    return Ok(());
                }
                Ok(Ok(ctx)) if ctx.agent == self.session.agent => {
                    self.lose_sight(since);
                    return Err(Error::Unavailable);
                }
                Ok(Err(Error::Unavailable)) | Err(_) => {
                    self.lose_sight(since);
                    let wake = (Instant::now() + self.pace()).min(deadline);
                    tokio::select! {
                        _ = self.gone.changed() => return Err(Error::Stale),
                        () = sleep_until(wake) => {}
                    }
                }
                Ok(_) => return Err(Error::Stale),
            }
        }
    }

    /// Waits for an ownership transition; cancel-safe across `select!` iterations.
    pub(super) async fn changed(&mut self) -> Change {
        let every = self.shared.cfg.revoke_check;
        let limit = every.max(Duration::from_secs(1));
        loop {
            if revoked(&self.gone) {
                return Change::Lost;
            }
            if self.deadline().is_some_and(|d| Instant::now() >= d) {
                return Change::Expired;
            }
            if self.inflight.is_none() {
                let wake = self
                    .deadline()
                    .map_or(self.next_check, |d| d.min(self.next_check));
                tokio::select! {
                    _ = self.gone.changed() => return Change::Lost,
                    () = sleep_until(wake) => {}
                }
                if self.deadline().is_some_and(|d| Instant::now() >= d) {
                    return Change::Expired;
                }
                let check: Reply<'a, RunContext> =
                    Box::pin(self.shared.parts.model.verify(self.session.attempt.clone()));
                self.inflight = Some((check, Instant::now()));
            }
            let Some(began) = self.inflight.as_ref().map(|(_, began)| *began) else {
                continue;
            };
            let cap = began + limit;
            let stop = self.deadline().map_or(cap, |d| d.min(cap));
            let Some((check, _)) = self.inflight.as_mut() else {
                continue;
            };
            let checked = tokio::select! {
                _ = self.gone.changed() => return Change::Lost,
                done = timeout_at(stop, &mut *check) => done.ok(),
            };
            self.inflight = None;
            // Treat late successes past `stop` as timeouts; accept late rejections (`Stale`/`Denied`).
            let checked = match checked {
                Some(Ok(_)) if Instant::now() >= stop => None,
                other => other,
            };
            if checked.is_none() && self.deadline().is_none_or(|d| Instant::now() < d) {
                self.lose_sight(began);
            }
            if checked.is_none() && self.deadline().is_some_and(|d| Instant::now() >= d) {
                return Change::Expired;
            }
            match checked {
                Some(Ok(ctx)) if ctx.agent == self.session.agent => {
                    let was = self.confirmed;
                    self.confirm();
                    if !was {
                        return Change::Confirmed;
                    }
                }
                Some(Err(Error::Unavailable)) | None => {
                    let was = self.confirmed;
                    self.lose_sight(began);
                    self.next_check = Instant::now() + self.pace();
                    if was {
                        return Change::Unconfirmed;
                    }
                }
                Some(_) => return Change::Lost,
            }
        }
    }
}
