//! Slack progress indicators (reactions and `chat.startStream` task cards) and thread stop controls.
//!
//! - Progress/reaction calls are best-effort and never block execution or reply delivery.
//! - Progress cards expose only fixed copy, role names, and tool names (never prompts, secrets, args, or drafts).
//! - Stop cancels the run tree via `RunAdmin::cancel` (audited) and is restricted to the requester or `admin`/`owner`.
mod cards;
mod stop;
mod stream;

pub use cards::{STOP_CONFIRM, STOP_CONTINUE, is_stop_phrase};
pub use stop::StopMention;

use super::creds::Credentials;
use bot_core::{
    id::{PrincipalId, ProfileId, RunId},
    port::{RunAdmin, RunStore},
    state::RunState,
};
use cards::{STOP_ACK, STOP_FINISHED, describe_progress, stop_card_blocks};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use stream::ThreadStream;
use tokio_util::sync::CancellationToken;

pub(super) const EYES: &str = "eyes";
const DONE: &str = "white_check_mark";
const FAIL: &str = "x";

const POLL_EVERY: Duration = Duration::from_millis(750);
const WATCH_LIMIT: Duration = Duration::from_secs(30 * 60);
const MAX_SEEN_RUNS: usize = 2048;

pub struct TrackedRun {
    pub profile: ProfileId,
    pub workspace: String,
    pub team: String,
    pub channel: String,
    pub thread_ts: String,
    pub msg_ts: String,
    pub user: String,
    pub principal: PrincipalId,
    pub run: RunId,
}

pub(super) struct RunUxState {
    pub(super) step: usize,
    pub(super) doing: String,
    pub(super) stop_asked: bool,
    pub(super) stop_in_flight: bool,
    pub(super) stop_confirmed: bool,
    pub(super) stop_ts: String,
    pub(super) stopped_snapshot: Option<(usize, String, u64)>,
}

pub(super) struct ActiveRun {
    pub(super) seq: u64,
    pub(super) info: TrackedRun,
    pub(super) started: Instant,
    pub(super) st: Mutex<RunUxState>,
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
        while self.order.len() > MAX_SEEN_RUNS {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

#[derive(Default)]
struct ActiveTable {
    // Keyed by RunId so overlapping runs in the same thread do not overwrite each other.
    by_run: HashMap<RunId, Arc<ActiveRun>>,
    // Ordered oldest-to-newest per thread so stop targets the most recent run.
    by_thread: HashMap<ThreadKey, Vec<RunId>>,
}

type ThreadKey = (String, String);
type StreamSlot = Arc<tokio::sync::Mutex<ThreadStream>>;

pub struct SlackUx {
    pub(super) creds: Arc<Credentials>,
    runs: Arc<dyn RunStore>,
    pub(super) admin: Arc<dyn RunAdmin>,
    poll_every: Duration,
    watch_limit: Duration,
    next_seq: AtomicU64,
    // Deduplicates duplicate events for the same run (e.g. app_mention + message.channels).
    seen: Mutex<SeenRuns>,
    active: Mutex<ActiveTable>,
    streams: Mutex<HashMap<ThreadKey, StreamSlot>>,
}

impl SlackUx {
    pub fn new(creds: Arc<Credentials>, runs: Arc<dyn RunStore>, admin: Arc<dyn RunAdmin>) -> Self {
        Self {
            creds,
            runs,
            admin,
            poll_every: POLL_EVERY,
            watch_limit: WATCH_LIMIT,
            next_seq: AtomicU64::new(1),
            seen: Mutex::new(SeenRuns::default()),
            active: Mutex::new(ActiveTable::default()),
            streams: Mutex::new(HashMap::new()),
        }
    }

    /// Test-only: overrides the progress poll interval and watch deadline.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub(crate) fn with_watch_timing(mut self, poll_every: Duration, watch_limit: Duration) -> Self {
        self.poll_every = poll_every;
        self.watch_limit = watch_limit;
        self
    }

    fn key(channel: &str, thread_ts: &str) -> ThreadKey {
        (channel.to_owned(), thread_ts.to_owned())
    }

    fn stream_slot(&self, channel: &str, thread_ts: &str) -> StreamSlot {
        let key = Self::key(channel, thread_ts);
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_default()
            .clone()
    }

    fn existing_stream_slot(&self, channel: &str, thread_ts: &str) -> Option<StreamSlot> {
        let key = Self::key(channel, thread_ts);
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned()
    }

    fn prune_stream_slot(&self, channel: &str, thread_ts: &str) {
        let key = Self::key(channel, thread_ts);
        let mut map = self.streams.lock().unwrap_or_else(|e| e.into_inner());
        let removable = map.get(&key).is_some_and(|slot| {
            Arc::strong_count(slot) == 1 && slot.try_lock().is_ok_and(|g| g.is_empty())
        });
        if removable {
            map.remove(&key);
        }
    }

    fn has_other_unclosed_active(
        &self,
        channel: &str,
        thread_ts: &str,
        run: &RunId,
        closed: &HashSet<RunId>,
    ) -> bool {
        let table = self.active.lock().unwrap_or_else(|e| e.into_inner());
        table
            .by_thread
            .get(&Self::key(channel, thread_ts))
            .is_some_and(|stack| stack.iter().any(|id| id != run && !closed.contains(id)))
    }

    fn remove_active(&self, info: &TrackedRun) {
        let mut table = self.active.lock().unwrap_or_else(|e| e.into_inner());
        table.by_run.remove(&info.run);
        let key = Self::key(&info.channel, &info.thread_ts);
        if let Some(stack) = table.by_thread.get_mut(&key) {
            stack.retain(|id| id != &info.run);
            if stack.is_empty() {
                table.by_thread.remove(&key);
            }
        }
    }

    pub(super) fn active_by_thread(
        &self,
        channel: &str,
        thread_ts: &str,
    ) -> Option<Arc<ActiveRun>> {
        let table = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let stack = table.by_thread.get(&Self::key(channel, thread_ts))?;
        stack
            .iter()
            .rev()
            .find_map(|id| table.by_run.get(id).cloned())
    }

    pub(super) fn active_by_run(&self, run_id: &str) -> Option<Arc<ActiveRun>> {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .by_run
            .get(&RunId(run_id.to_owned()))
            .cloned()
    }

    /// Starts background progress tracking (non-blocking for Slack ack).
    /// Returns `false` on duplicate `run` calls (e.g. `app_mention` + `message.channels`).
    pub fn track(self: &Arc<Self>, info: TrackedRun, stop: CancellationToken) -> bool {
        {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.claim(info.run.clone()) {
                return false;
            }
        }
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let entry = Arc::new(ActiveRun {
            seq,
            info,
            started: Instant::now(),
            st: Mutex::new(RunUxState {
                step: 0,
                doing: "Waiting in queue".into(),
                stop_asked: false,
                stop_in_flight: false,
                stop_confirmed: false,
                stop_ts: String::new(),
                stopped_snapshot: None,
            }),
        });
        {
            let key = Self::key(&entry.info.channel, &entry.info.thread_ts);
            let run = entry.info.run.clone();
            let mut table = self.active.lock().unwrap_or_else(|e| e.into_inner());
            table.by_run.insert(run.clone(), entry.clone());
            table.by_thread.entry(key).or_default().push(run);
        }
        let ux = self.clone();
        tokio::spawn(async move {
            ux.watch_run(entry, stop).await;
        });
        true
    }

    async fn watch_run(self: Arc<Self>, active: Arc<ActiveRun>, stop: CancellationToken) {
        let info = &active.info;
        // Skip reactions and status cards for redelivered events of runs already in a terminal state.
        if let Ok(Some(progress)) = self
            .runs
            .progress(info.profile.clone(), info.run.clone())
            .await
            && matches!(
                progress.state,
                RunState::Succeeded | RunState::Dead | RunState::Cancelled
            )
        {
            self.remove_active(info);
            self.retire_run(&info.channel, &info.thread_ts, &info.run, false)
                .await;
            return;
        }
        self.react(
            &info.profile,
            &info.workspace,
            &info.channel,
            &info.msg_ts,
            EYES,
        )
        .await;

        let mut final_state: Option<RunState> = None;
        let deadline = tokio::time::Instant::now() + self.watch_limit;
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
            let elapsed_s = active.started.elapsed().as_secs();
            let (step, card_text, doing) = describe_progress(&progress, elapsed_s);
            {
                let mut st = active.st.lock().unwrap_or_else(|e| e.into_inner());
                st.step = step;
                st.doing = doing;
            }
            if matches!(
                progress.state,
                RunState::Succeeded | RunState::Dead | RunState::Cancelled
            ) {
                final_state = Some(progress.state);
                break;
            }
            self.update_status(active.seq, info, card_text.as_deref())
                .await;
        }

        // Deregister before retiring this run's stream, and only set a terminal reaction on confirmed completion.
        self.remove_active(info);
        self.retire_run(
            &info.channel,
            &info.thread_ts,
            &info.run,
            !stop.is_cancelled(),
        )
        .await;

        match final_state {
            Some(RunState::Succeeded) => {
                self.settle_stop_notice(&active, STOP_FINISHED).await;
                self.finish_reaction(
                    &info.profile,
                    &info.workspace,
                    &info.channel,
                    &info.msg_ts,
                    DONE,
                )
                .await;
            }
            Some(RunState::Cancelled) => {
                self.settle_stop_notice(&active, STOP_ACK).await;
                let (step, doing, elapsed_s) = {
                    let mut st = active.st.lock().unwrap_or_else(|e| e.into_inner());
                    st.stopped_snapshot.take().unwrap_or_else(|| {
                        (
                            st.step,
                            st.doing.clone(),
                            active.started.elapsed().as_secs(),
                        )
                    })
                };
                let (blocks, fallback) = stop_card_blocks(step, &doing, elapsed_s, true, None);
                self.post_blocks(
                    &info.profile,
                    &info.workspace,
                    &info.channel,
                    &info.thread_ts,
                    blocks,
                    &fallback,
                )
                .await;
                self.finish_reaction(
                    &info.profile,
                    &info.workspace,
                    &info.channel,
                    &info.msg_ts,
                    FAIL,
                )
                .await;
            }
            Some(RunState::Dead) => {
                self.settle_stop_notice(&active, STOP_FINISHED).await;
                self.finish_reaction(
                    &info.profile,
                    &info.workspace,
                    &info.channel,
                    &info.msg_ts,
                    FAIL,
                )
                .await;
            }
            // Watch deadline or graceful shutdown: run may still be active, so do not react with :x:.
            None | Some(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seen_runs_deduplicates_duplicate_events_and_bounds_size() {
        let mut seen = SeenRuns::default();
        assert!(seen.claim(RunId("r1".into())));
        assert!(!seen.claim(RunId("r1".into())));
        assert!(seen.claim(RunId("r2".into())));
        for i in 0..MAX_SEEN_RUNS {
            assert!(seen.claim(RunId(format!("fill-{i}"))));
        }
        assert!(seen.set.len() <= MAX_SEEN_RUNS);
        assert!(seen.claim(RunId("r1".into())));
    }
}
