//! Telegram progress feedback (eyes reaction, typing heartbeat) and bounded reply-target tracking.
//!
//! All reaction and chat-action calls are strictly best-effort and never block intake or reply delivery.
use super::creds::Credentials;
use bot_core::{
    id::{ProfileId, RunId},
    port::RunStore,
    state::RunState,
};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// Verified emojis from Telegram Bot API `ReactionTypeEmoji`.
pub(crate) const EYES: &str = "👀";
pub(crate) const DONE: &str = "👍";

const POLL_EVERY: Duration = Duration::from_secs(1);
const TYPING_EVERY: Duration = Duration::from_secs(4);
const WATCH_LIMIT: Duration = Duration::from_secs(30 * 60);
const MAX_TRACKED: usize = 2048;

pub(crate) struct TrackedRun {
    pub profile: ProfileId,
    pub alias: String,
    pub chat_id: i64,
    pub message_id: u64,
    pub thread_id: Option<u64>,
    pub run: RunId,
}

#[derive(Default)]
struct SeenRuns {
    set: HashSet<RunId>,
    order: VecDeque<RunId>,
}

impl SeenRuns {
    fn claim(&mut self, run: RunId) -> bool {
        if !self.set.insert(run.clone()) {
            return false;
        }
        self.order.push_back(run);
        while self.order.len() > MAX_TRACKED {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

/// Bounded in-memory map from accepted `RunId` to triggering Telegram `message_id` for group `reply_parameters`.
#[derive(Default)]
pub(crate) struct ReplyTargets {
    inner: Mutex<ReplyTargetMap>,
}

#[derive(Default)]
struct ReplyTargetMap {
    map: HashMap<RunId, u64>,
    order: VecDeque<RunId>,
}

impl ReplyTargets {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn record(&self, run: RunId, message_id: u64) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.map.insert(run.clone(), message_id).is_none() {
            inner.order.push_back(run);
            while inner.order.len() > MAX_TRACKED {
                if let Some(old) = inner.order.pop_front() {
                    inner.map.remove(&old);
                }
            }
        }
    }

    pub(crate) fn get(&self, run: &RunId) -> Option<u64> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .get(run)
            .copied()
    }
}

pub(crate) struct TelegramUx {
    creds: Arc<Credentials>,
    runs: Arc<dyn RunStore>,
    poll_every: Duration,
    typing_every: Duration,
    watch_limit: Duration,
    seen: Mutex<SeenRuns>,
}

impl TelegramUx {
    pub(crate) fn new(creds: Arc<Credentials>, runs: Arc<dyn RunStore>) -> Self {
        Self {
            creds,
            runs,
            poll_every: POLL_EVERY,
            typing_every: TYPING_EVERY,
            watch_limit: WATCH_LIMIT,
            seen: Mutex::new(SeenRuns::default()),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub(crate) fn with_timing(
        mut self,
        poll_every: Duration,
        typing_every: Duration,
        watch_limit: Duration,
    ) -> Self {
        self.poll_every = poll_every;
        self.typing_every = typing_every;
        self.watch_limit = watch_limit;
        self
    }

    /// Starts background reaction and typing lifecycle tracking; returns false if `run` was already tracked.
    pub(crate) fn track(self: &Arc<Self>, info: TrackedRun, stop: CancellationToken) -> bool {
        {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.claim(info.run.clone()) {
                return false;
            }
        }
        let ux = self.clone();
        tokio::spawn(async move {
            ux.watch_run(info, stop).await;
        });
        true
    }

    async fn watch_run(self: Arc<Self>, info: TrackedRun, stop: CancellationToken) {
        if let Ok(Some(progress)) = self
            .runs
            .progress(info.profile.clone(), info.run.clone())
            .await
            && matches!(
                progress.state,
                RunState::Succeeded | RunState::Dead | RunState::Cancelled
            )
        {
            return;
        }
        self.set_reaction(&info, Some(EYES)).await;

        let deadline = tokio::time::Instant::now() + self.watch_limit;
        let mut last_typing: Option<Instant> = None;
        let mut final_state: Option<RunState> = None;
        let mut first = true;

        while tokio::time::Instant::now() < deadline {
            if first {
                first = false;
                if stop.is_cancelled() {
                    break;
                }
            } else {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep_until(deadline) => break,
                    _ = tokio::time::sleep(self.poll_every) => {}
                }
            }
            let progress = tokio::select! {
                _ = stop.cancelled() => break,
                _ = tokio::time::sleep_until(deadline) => break,
                res = self.runs.progress(info.profile.clone(), info.run.clone()) => res,
            };
            let Ok(Some(progress)) = progress else {
                continue;
            };
            if matches!(
                progress.state,
                RunState::Succeeded | RunState::Dead | RunState::Cancelled
            ) {
                final_state = Some(progress.state);
                break;
            }
            let active = matches!(progress.state, RunState::Ready | RunState::Leased)
                && !progress.waiting_approval;
            if active && last_typing.is_none_or(|t| t.elapsed() >= self.typing_every) {
                self.send_typing(&info).await;
                last_typing = Some(Instant::now());
            }
        }

        match final_state {
            Some(RunState::Succeeded) => self.set_reaction(&info, Some(DONE)).await,
            Some(RunState::Dead | RunState::Cancelled) => self.set_reaction(&info, None).await,
            None | Some(_) => {}
        }
    }

    async fn set_reaction(&self, info: &TrackedRun, emoji: Option<&str>) {
        let Some(token) = self.creds.bot_token(&info.profile, &info.alias).await else {
            return;
        };
        let _ = self
            .creds
            .api()
            .set_message_reaction(&token, info.chat_id, info.message_id, emoji)
            .await;
    }

    async fn send_typing(&self, info: &TrackedRun) {
        let Some(token) = self.creds.bot_token(&info.profile, &info.alias).await else {
            return;
        };
        let mut body = json!({"chat_id": info.chat_id, "action": "typing"});
        if let Some(thread_id) = info.thread_id {
            body["message_thread_id"] = json!(thread_id);
        }
        let _ = self.creds.call_json("sendChatAction", &token, &body).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_targets_and_seen_runs_stay_bounded() {
        let targets = ReplyTargets::new();
        targets.record(RunId("r1".into()), 10);
        assert_eq!(targets.get(&RunId("r1".into())), Some(10));
        for i in 0..MAX_TRACKED {
            targets.record(RunId(format!("fill-{i}")), i as u64 + 1);
        }
        assert_eq!(targets.get(&RunId("r1".into())), None);
        assert_eq!(targets.get(&RunId("fill-0".into())), Some(1));

        let mut seen = SeenRuns::default();
        assert!(seen.claim(RunId("a".into())));
        assert!(!seen.claim(RunId("a".into())));
    }
}
