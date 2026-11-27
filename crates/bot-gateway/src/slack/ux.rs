//! Slack 진행 표시(👀/✅ 반응 · `chat.startStream` task_update 카드)와 스레드 중단(Agent 중단 버튼 · "중단해줘" 멘션 · 확인 카드).
//!
//! - 모든 상태 표시·반응은 장식이라 실패해도 본 실행이나 회신 전달을 막지 않는다.
//! - 진행 카드에는 고정 문구·역할명·도구명만 싣고, 사용자 원문·비밀·도구 인자·모델 초안은 싣지 않는다.
//! - 중단은 `RunAdmin::cancel`로 저장소·자식 run 트리를 취소하고 감사에 남긴다. 요청한 본인이나 `admin`/`owner` 역할만 중단할 수 있다.
mod cards;
mod stop;

pub use cards::{STOP_CONFIRM, STOP_CONTINUE, is_stop_phrase};
pub use stop::StopMention;

use super::{
    api::{Body, Raw},
    creds::Credentials,
};
use bot_core::{
    id::{PrincipalId, ProfileId, RunId},
    port::{RunAdmin, RunStore},
    state::RunState,
};
use cards::{STOP_ACK, STOP_FINISHED, describe_progress, stop_card_blocks};
use serde_json::json;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

pub(super) const EYES: &str = "eyes";
const DONE: &str = "white_check_mark";
const FAIL: &str = "x";

const POLL_EVERY: Duration = Duration::from_millis(750);
const WATCH_LIMIT: Duration = Duration::from_secs(30 * 60);
const MAX_SEEN_RUNS: usize = 2048;

/// 추적할 Slack 요청 한 건의 정보.
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
    /// 처음 보는 run이면 기록하고 `true`, 이미 추적했거나 진행 중인 run이면 `false`.
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
    /// `RunId` → 현재 추적 중인 run (같은 스레드에 둘 이상 겹쳐도 덮어쓰지 않는다)
    by_run: HashMap<RunId, Arc<ActiveRun>>,
    /// `(channel, thread_ts)` → 스레드별 추적 중인 run 순서(가장 최근 것이 뒤)
    by_thread: HashMap<ThreadKey, Vec<RunId>>,
}

#[derive(Default)]
struct ThreadStream {
    /// 현재 열려 있는 진행 스트림 `(run, ts)`
    open: Option<(RunId, String)>,
    /// 최종 회신이 시작되었거나 끝난 run 집합 — 뒤늦은 폴링이 답글 뒤에 카드를 다시 열지 못하게 막는다.
    closed: HashSet<RunId>,
}

type ThreadKey = (String, String);
type StreamSlot = Arc<tokio::sync::Mutex<ThreadStream>>;

/// Slack 진행 상태(`chat.startStream` · 👀/✅)와 스레드 중단 제어기.
pub struct SlackUx {
    pub(super) creds: Arc<Credentials>,
    runs: Arc<dyn RunStore>,
    pub(super) admin: Arc<dyn RunAdmin>,
    poll_every: Duration,
    watch_limit: Duration,
    /// 같은 run(`app_mention` + `message.channels` 중복 수신 등)이 두 번 추적되지 않게 막는 집합
    seen: Mutex<SeenRuns>,
    /// 실행별·스레드별 현재 추적 중인 run
    active: Mutex<ActiveTable>,
    /// `(channel, thread_ts)` → 스레드별 직렬화된 진행 스트림 상태
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
            seen: Mutex::new(SeenRuns::default()),
            active: Mutex::new(ActiveTable::default()),
            streams: Mutex::new(HashMap::new()),
        }
    }

    /// 시험 전용: 상태 폴링 간격과 감시 상한을 줄인다.
    #[doc(hidden)]
    #[must_use]
    pub fn with_watch_timing(mut self, poll_every: Duration, watch_limit: Duration) -> Self {
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
            Arc::strong_count(slot) == 1
                && slot
                    .try_lock()
                    .is_ok_and(|g| g.open.is_none() && g.closed.is_empty())
        });
        if removable {
            map.remove(&key);
        }
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

    /// 접수된 Slack 요청의 진행 추적 시작(비동기 백그라운드 작업으로 돌려 ack를 늦추지 않는다).
    /// 같은 `run`에 대한 중복 호출(`app_mention`과 `message.channels` 동시 수신 등)은 무시하고 `false`를 돌려준다.
    pub fn track(self: &Arc<Self>, info: TrackedRun, stop: CancellationToken) -> bool {
        {
            let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
            if !seen.claim(info.run.clone()) {
                return false;
            }
        }
        let entry = Arc::new(ActiveRun {
            info,
            started: Instant::now(),
            st: Mutex::new(RunUxState {
                step: 0,
                doing: "순서를 기다리는 중".into(),
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
        // 재시작 뒤 이미 끝난 run의 재전달이면 반응·상태 카드를 띄우지 않는다.
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

        let mut last_card: Option<String> = None;
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
            if let Some(card_text) = card_text
                && last_card.as_deref() != Some(card_text.as_str())
            {
                self.set_status(
                    &info.profile,
                    &info.workspace,
                    &info.channel,
                    &info.thread_ts,
                    &info.run,
                    &card_text,
                    &info.team,
                    &info.user,
                )
                .await;
                last_card = Some(card_text);
            }
        }

        // 정리: 활성 목록에서 먼저 빼고, 이 run의 진행 카드·닫힘 표시만 치운 뒤, 확정된 종결 상태일 때만 결과 반응을 남긴다.
        self.remove_active(info);
        self.clear_stream_for_run(
            &info.profile,
            &info.workspace,
            &info.channel,
            &info.thread_ts,
            &info.run,
            true,
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
            // 감시 기한 만료나 정상 종료: 아직 진행 중일 수 있는 작업에 `:x:`를 달지 않는다.
            None | Some(_) => {}
        }
    }

    async fn close_and_delete_stream(
        &self,
        profile: &ProfileId,
        workspace: &str,
        channel: &str,
        ts: &str,
    ) {
        let Some(token) = self.creds.bot_token(profile, workspace).await else {
            return;
        };
        let stop_body = json!({"channel": channel, "ts": ts, "markdown_text": ""});
        let _ = self
            .creds
            .call_passive("chat.stopStream", &token, Body::Json(&stop_body))
            .await;
        let del_body = json!({"channel": channel, "ts": ts});
        let _ = self
            .creds
            .call_passive("chat.delete", &token, Body::Json(&del_body))
            .await;
    }

    /// 회신 게시 직전(`SlackSender`)에 **이 `run`이 연** 진행 스트림 카드만 닫고 지운다.
    /// 같은 스레드에 겹쳐 돌고 있는 다른 run의 카드는 건드리지 않는다.
    /// `mark_closed`가 참이면(최종 회신) 아직 감시 중인 이 run이 답글 뒤에 새 카드를 다시 열지 못하게 잠근다.
    pub async fn clear_stream(
        &self,
        profile: &ProfileId,
        workspace: &str,
        channel: &str,
        thread_ts: &str,
        run: &RunId,
        mark_closed: bool,
    ) {
        let slot = if mark_closed && self.active_by_run(&run.0).is_some() {
            Some(self.stream_slot(channel, thread_ts))
        } else {
            self.existing_stream_slot(channel, thread_ts)
        };
        let Some(slot) = slot else { return };
        let mut guard = slot.lock().await;
        let ts = if guard.open.as_ref().is_some_and(|(r, _)| r == run) {
            guard.open.take().map(|(_, ts)| ts)
        } else {
            None
        };
        if mark_closed && self.active_by_run(&run.0).is_some() {
            guard.closed.insert(run.clone());
        }
        if let Some(ts) = ts {
            self.close_and_delete_stream(profile, workspace, channel, &ts)
                .await;
        }
        drop(guard);
        drop(slot);
        self.prune_stream_slot(channel, thread_ts);
    }

    async fn clear_stream_for_run(
        &self,
        profile: &ProfileId,
        workspace: &str,
        channel: &str,
        thread_ts: &str,
        run: &RunId,
        retire: bool,
    ) {
        let Some(slot) = self.existing_stream_slot(channel, thread_ts) else {
            return;
        };
        let mut guard = slot.lock().await;
        let ts = if guard.open.as_ref().is_some_and(|(r, _)| r == run) {
            guard.open.take().map(|(_, ts)| ts)
        } else {
            None
        };
        if retire {
            guard.closed.remove(run);
        }
        if let Some(ts) = ts {
            self.close_and_delete_stream(profile, workspace, channel, &ts)
                .await;
        }
        drop(guard);
        drop(slot);
        self.prune_stream_slot(channel, thread_ts);
    }

    #[allow(clippy::too_many_arguments)]
    async fn set_status(
        &self,
        profile: &ProfileId,
        workspace: &str,
        channel: &str,
        thread_ts: &str,
        run: &RunId,
        status: &str,
        team_id: &str,
        user_id: &str,
    ) {
        if status.is_empty() {
            self.clear_stream_for_run(profile, workspace, channel, thread_ts, run, false)
                .await;
            return;
        }
        let Some(token) = self.creds.bot_token(profile, workspace).await else {
            return;
        };
        let slot = self.stream_slot(channel, thread_ts);
        let mut guard = slot.lock().await;
        if guard.closed.contains(run) {
            return;
        }
        // 같은 스레드에 이전 run의 열린 카드가 남아 있으면 먼저 지운다.
        if guard.open.as_ref().is_some_and(|(r, _)| r != run)
            && let Some((_, old_ts)) = guard.open.take()
        {
            self.close_and_delete_stream(profile, workspace, channel, &old_ts)
                .await;
        }
        let chunk = json!({
            "type": "task_update",
            "id": "status",
            "title": status,
            "status": "in_progress"
        });
        match guard.open.as_ref().map(|(_, ts)| ts.clone()) {
            Some(ts) => {
                let body = json!({"channel": channel, "ts": ts, "chunks": [chunk]});
                match self
                    .creds
                    .call_passive("chat.appendStream", &token, Body::Json(&body))
                    .await
                {
                    Raw::Ok(_) => {}
                    _ => {
                        guard.open = None;
                        let del = json!({"channel": channel, "ts": ts});
                        let _ = self
                            .creds
                            .call_passive("chat.delete", &token, Body::Json(&del))
                            .await;
                    }
                }
            }
            None => {
                let mut body = json!({
                    "channel": channel,
                    "thread_ts": thread_ts,
                    "chunks": [chunk]
                });
                if !team_id.is_empty() {
                    body["recipient_team_id"] = json!(team_id);
                }
                if !user_id.is_empty() {
                    body["recipient_user_id"] = json!(user_id);
                }
                if let Raw::Ok(val) = self
                    .creds
                    .call_passive("chat.startStream", &token, Body::Json(&body))
                    .await
                    && let Some(ts) = val["ts"].as_str()
                {
                    guard.open = Some((run.clone(), ts.to_owned()));
                }
            }
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
