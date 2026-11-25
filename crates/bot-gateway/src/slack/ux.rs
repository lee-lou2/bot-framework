//! Slack 진행 표시(👀/✅ 반응 · `chat.startStream` task_update 카드)와 스레드 중단(Agent 중단 버튼 · "중단해줘" 멘션 · 확인 카드).
//!
//! - 모든 상태 표시·반응은 장식이라 실패해도 본 실행이나 회신 전달을 막지 않는다.
//! - 진행 카드에는 고정 문구·역할명·도구명만 싣고, 사용자 원문·비밀·도구 인자·모델 초안은 싣지 않는다.
//! - 중단은 `RunAdmin::cancel`로 저장소·자식 run 트리를 취소하고 감사에 남긴다. 요청한 본인이나 `admin`/`owner` 역할만 중단할 수 있다.
use super::{
    api::{Body, Raw},
    creds::Credentials,
    frame::Button,
    socket::Installation,
};
use crate::identity;
use bot_core::{
    id::{PrincipalId, ProfileId, RunId},
    intake::RunProgress,
    port::{RunAdmin, RunStore},
    profile::Snapshot,
    state::RunState,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub const STOP_CONFIRM: &str = "bot_stop_confirm";
pub const STOP_CONTINUE: &str = "bot_stop_continue";

const EYES: &str = "eyes";
const DONE: &str = "white_check_mark";
const FAIL: &str = "x";

const POLL_EVERY: Duration = Duration::from_millis(750);
const WATCH_LIMIT: Duration = Duration::from_secs(30 * 60);
const MAX_SEEN_RUNS: usize = 2048;

const STEPS: [&str; 4] = ["요청 확인", "맥락 파악", "답 작성", "전송"];
const STOP_NOTE: &str = "버튼이 안 보이면 이 스레드에서 저를 멘션하고 \"중단해줘\"라고 보내도 돼요";
const STOP_ACK: &str = "🛑 중단을 요청하셨어요. 결과는 아래에 남길게요.";
const STOP_RESUMED: &str = "▶️ 중단하지 않고 계속 진행할게요.";
const STOP_FINISHED: &str = "✔️ 묻는 사이 작업이 끝났어요.";
const STOP_GONE: &str = "이미 끝났거나 지난 확인이라 중단할 작업이 없어요.";

const AUTO_SEND_NOTICE: &str = "자동 발송";

/// 글이 **중단 명령 그것뿐**인가 ("중단해줘" · "멈춰!"). 공백 · 문장부호는 무시한다.
/// 다른 말이 섞이면("멈춰있는 서버 원인 알려줘", "중단하지 마") 중단이 아니라 새 질문이다.
pub fn is_stop_phrase(text: &str) -> bool {
    const COMMANDS: [&str; 7] = [
        "중단",
        "중단해",
        "중단해줘",
        "중단해주세요",
        "멈춰",
        "멈춰줘",
        "멈춰주세요",
    ];
    let text = text.split(AUTO_SEND_NOTICE).next().unwrap_or_default();
    let t: String = text.chars().filter(|c| c.is_alphanumeric()).collect();
    COMMANDS.contains(&t.as_str())
}

fn elapsed_label(secs: u64) -> String {
    if secs >= 60 {
        format!("{}분 {}초", secs / 60, secs % 60)
    } else {
        format!("{secs}초")
    }
}

/// 단계 막대: 끝낸 단계 🟩, 지금(멈췄으면 🟥) 단계 🟨, 남은 단계 ⬜.
fn stepper(step: usize, stopped: bool) -> String {
    let now = if stopped { "🟥" } else { "🟨" };
    STEPS
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let mark = if i < step {
                "🟩"
            } else if i == step {
                now
            } else {
                "⬜"
            };
            format!("{mark} {name}")
        })
        .collect::<Vec<_>>()
        .join("  ›  ")
}

/// 중단 확인(stopped=false) 또는 중단 결과(stopped=true) Block Kit 블록과 요약 글.
fn stop_card_blocks(
    step: usize,
    doing: &str,
    elapsed_s: u64,
    stopped: bool,
    run_id: Option<&str>,
) -> (Vec<Value>, String) {
    let bar = stepper(step, stopped);
    let (title, body, note) = if stopped {
        let did = match step {
            0 | 1 => "버린 결과물은 없어요.",
            2 => "쓰던 답은 버렸고 올리지 않았어요.",
            _ => "다 쓴 답은 버렸고 올리지 않았어요.",
        };
        (
            "🛑 중단했어요",
            format!("{bar}\n\n{doing}에 멈췄어요 · {} 진행", elapsed_label(elapsed_s)),
            format!("{did} 다시 필요하면 새로 멘션해 주세요."),
        )
    } else {
        let will = match step {
            0 | 1 => "지금 중단해도 버려질 결과물은 아직 없어요.",
            2 => "지금 중단하면 쓰던 답이 버려지고, 아무것도 올라가지 않아요.",
            _ => "지금 중단하면 다 쓴 답이 버려져요. 거의 다 됐어요.",
        };
        (
            "⏳ 작업이 진행 중이에요",
            format!("{bar}\n\n{doing}이에요 · {}째\n\n{will}", elapsed_label(elapsed_s)),
            STOP_NOTE.to_owned(),
        )
    };
    let mut blocks = vec![
        json!({"type": "header", "text": {"type": "plain_text", "text": title, "emoji": true}}),
        json!({"type": "section", "text": {"type": "mrkdwn", "text": body}}),
    ];
    if let Some(id) = run_id
        && !stopped
    {
        blocks.push(json!({
            "type": "actions",
            "elements": [
                {
                    "type": "button",
                    "action_id": STOP_CONFIRM,
                    "style": "danger",
                    "text": {"type": "plain_text", "text": "여기서 중단", "emoji": true},
                    "value": id
                },
                {
                    "type": "button",
                    "action_id": STOP_CONTINUE,
                    "text": {"type": "plain_text", "text": "계속 진행", "emoji": true},
                    "value": id
                }
            ]
        }));
    }
    blocks.push(json!({
        "type": "context",
        "elements": [{"type": "mrkdwn", "text": note}]
    }));
    (blocks, format!("{title} — {doing}"))
}

/// `RunProgress`를 진행 카드 문구(`None`이면 카드 미표시)·중단 카드 단계로 바꾼다(순수).
fn describe_progress(progress: &RunProgress, elapsed_s: u64) -> (usize, Option<String>, String) {
    if progress.cancel_requested {
        return (2, Some("중단하고 있어요".into()), "중단하는 중".into());
    }
    if progress.waiting_approval {
        return match &progress.active_tool {
            Some(tool) => (
                2,
                Some(format!("승인을 기다리고 있어요 ({tool})")),
                format!("도구({tool}) 승인 대기 중"),
            ),
            None => (
                2,
                Some("승인을 기다리고 있어요".into()),
                "승인 대기 중".into(),
            ),
        };
    }
    if let Some(tool) = &progress.active_tool {
        return (
            2,
            Some(format!("도구 실행 중: {tool}")),
            format!("도구({tool}) 실행 중"),
        );
    }
    if let Some(agent) = &progress.active_agent {
        let short = agent.rsplit('/').next().unwrap_or(agent);
        return (
            2,
            Some(format!("{short} 역할에서 작업하고 있어요")),
            format!("{short} 역할에서 작업 중"),
        );
    }
    match progress.state {
        RunState::Ready => (0, None, "순서를 기다리는 중".into()),
        RunState::Retry => (
            2,
            Some("잠시 막혀서 다시 시도하고 있어요".into()),
            "막혀서 다시 시도하는 중".into(),
        ),
        RunState::Waiting => (
            2,
            Some("하위 작업을 기다리고 있어요".into()),
            "하위 작업 대기 중".into(),
        ),
        RunState::Leased if elapsed_s >= 15 => (
            2,
            Some("깊이 생각하고 있어요".into()),
            "깊이 생각하는 중".into(),
        ),
        RunState::Leased => (
            2,
            Some("답을 만들고 있어요".into()),
            "답을 만드는 중".into(),
        ),
        RunState::Succeeded | RunState::Dead | RunState::Cancelled => {
            (3, None, "마무리 중".into())
        }
    }
}

fn can_stop(snapshot: Option<&Snapshot>, actor: &PrincipalId, requester: &PrincipalId) -> bool {
    if actor == requester {
        return true;
    }
    snapshot
        .and_then(|s| s.members.get(actor))
        .is_some_and(|roles| roles.iter().any(|r| matches!(r.as_str(), "admin" | "owner")))
}

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

struct RunUxState {
    step: usize,
    doing: String,
    stop_asked: bool,
    stop_confirmed: bool,
    stop_ts: String,
    stopped_snapshot: Option<(usize, String, u64)>,
}

struct ActiveRun {
    info: TrackedRun,
    started: Instant,
    st: Mutex<RunUxState>,
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
struct ThreadStream {
    /// 현재 열려 있는 진행 스트림 `(run, ts)`
    open: Option<(RunId, String)>,
    /// 최종 회신이 시작되었거나 끝난 run — 뒤늦은 폴링이 답글 뒤에 카드를 다시 열지 못하게 막는다.
    closed_for: Option<RunId>,
}

/// Slack 진행 상태(`chat.startStream` · 👀/✅)와 스레드 중단 제어기.
pub struct SlackUx {
    creds: Arc<Credentials>,
    runs: Arc<dyn RunStore>,
    admin: Arc<dyn RunAdmin>,
    /// 같은 run(`app_mention` + `message.channels` 중복 수신 등)이 두 번 추적되지 않게 막는 집합
    seen: Mutex<SeenRuns>,
    /// `(channel, thread_ts)` → 현재 추적 중인 run
    active: Mutex<HashMap<(String, String), Arc<ActiveRun>>>,
    /// `(channel, thread_ts)` → 스레드별 직렬화된 진행 스트림 상태
    streams: Mutex<HashMap<(String, String), Arc<tokio::sync::Mutex<ThreadStream>>>>,
}

impl SlackUx {
    pub fn new(
        creds: Arc<Credentials>,
        runs: Arc<dyn RunStore>,
        admin: Arc<dyn RunAdmin>,
    ) -> Self {
        Self {
            creds,
            runs,
            admin,
            seen: Mutex::new(SeenRuns::default()),
            active: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
        }
    }

    fn key(channel: &str, thread_ts: &str) -> (String, String) {
        (channel.to_owned(), thread_ts.to_owned())
    }

    fn stream_slot(&self, channel: &str, thread_ts: &str) -> Arc<tokio::sync::Mutex<ThreadStream>> {
        let key = Self::key(channel, thread_ts);
        self.streams
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(key)
            .or_default()
            .clone()
    }

    fn existing_stream_slot(
        &self,
        channel: &str,
        thread_ts: &str,
    ) -> Option<Arc<tokio::sync::Mutex<ThreadStream>>> {
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
            Arc::strong_count(slot) == 1 && slot.try_lock().is_ok_and(|g| g.open.is_none())
        });
        if removable {
            map.remove(&key);
        }
    }

    fn remove_active(&self, info: &TrackedRun) {
        let mut map = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let key = Self::key(&info.channel, &info.thread_ts);
        if map.get(&key).is_some_and(|cur| cur.info.run == info.run) {
            map.remove(&key);
        }
    }

    fn active_by_thread(&self, channel: &str, thread_ts: &str) -> Option<Arc<ActiveRun>> {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&Self::key(channel, thread_ts))
            .cloned()
    }

    fn active_by_run(&self, run_id: &str) -> Option<Arc<ActiveRun>> {
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|r| r.info.run.0 == run_id)
            .cloned()
    }

    /// 접수된 Slack 요청의 진행 추적 시작(비동기 백그라운드 작업으로 돌려 ack를 늦추지 않는다).
    /// 같은 `run`에 대한 중복 호출(`app_mention`과 `message.channels` 동시 수신 등)은 무시하고 `false`를 돌려준다.
    pub fn track(self: &Arc<Self>, info: TrackedRun) -> bool {
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
                stop_confirmed: false,
                stop_ts: String::new(),
                stopped_snapshot: None,
            }),
        });
        let key = Self::key(&entry.info.channel, &entry.info.thread_ts);
        self.active
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, entry.clone());
        let ux = self.clone();
        tokio::spawn(async move {
            ux.watch_run(entry).await;
        });
        true
    }

    async fn watch_run(self: Arc<Self>, active: Arc<ActiveRun>) {
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
        self.react(&info.profile, &info.workspace, &info.channel, &info.msg_ts, EYES)
            .await;

        let mut last_card: Option<String> = None;
        let mut final_state = RunState::Dead;
        let deadline = Instant::now() + WATCH_LIMIT;
        let mut first = true;
        while Instant::now() < deadline {
            if first {
                first = false;
            } else {
                tokio::time::sleep(POLL_EVERY).await;
            }
            let Ok(Some(progress)) = self
                .runs
                .progress(info.profile.clone(), info.run.clone())
                .await
            else {
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
                final_state = progress.state;
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

        // 정리: 진행 카드를 먼저 치우고, 활성 목록에서 뺀 뒤, 결과 반응·중단 카드를 마무리한다.
        self.clear_stream_for_run(
            &info.profile,
            &info.workspace,
            &info.channel,
            &info.thread_ts,
            &info.run,
        )
        .await;
        self.remove_active(info);
        self.prune_stream_slot(&info.channel, &info.thread_ts);

        match final_state {
            RunState::Succeeded => {
                self.settle_stop_notice(&active, STOP_FINISHED).await;
                self.finish_reaction(&info.profile, &info.workspace, &info.channel, &info.msg_ts, DONE)
                    .await;
            }
            RunState::Cancelled => {
                self.settle_stop_notice(&active, STOP_ACK).await;
                let (step, doing, elapsed_s) = {
                    let mut st = active.st.lock().unwrap_or_else(|e| e.into_inner());
                    st.stopped_snapshot.take().unwrap_or_else(|| {
                        (st.step, st.doing.clone(), active.started.elapsed().as_secs())
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
                self.finish_reaction(&info.profile, &info.workspace, &info.channel, &info.msg_ts, FAIL)
                    .await;
            }
            _ => {
                self.settle_stop_notice(&active, STOP_FINISHED).await;
                self.finish_reaction(&info.profile, &info.workspace, &info.channel, &info.msg_ts, FAIL)
                    .await;
            }
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
            .call("chat.stopStream", &token, Body::Json(&stop_body))
            .await;
        let del_body = json!({"channel": channel, "ts": ts});
        let _ = self
            .creds
            .call("chat.delete", &token, Body::Json(&del_body))
            .await;
    }

    /// 회신 게시 직전(`SlackSender`)에 열려 있는 진행 스트림 카드를 닫고 지운다.
    /// `mark_closed`가 참이면(최종 회신) 진행 중이던 run이 답글 뒤에 새 카드를 다시 열지 못하게 잠근다.
    pub async fn clear_stream(
        &self,
        profile: &ProfileId,
        workspace: &str,
        channel: &str,
        thread_ts: &str,
        mark_closed: bool,
    ) {
        let active_run = self
            .active_by_thread(channel, thread_ts)
            .map(|a| a.info.run.clone());
        let slot = if mark_closed && active_run.is_some() {
            Some(self.stream_slot(channel, thread_ts))
        } else {
            self.existing_stream_slot(channel, thread_ts)
        };
        let Some(slot) = slot else { return };
        let mut guard = slot.lock().await;
        let open = guard.open.take();
        if mark_closed
            && let Some(run) = open.as_ref().map(|(r, _)| r.clone()).or(active_run)
        {
            guard.closed_for = Some(run);
        }
        if let Some((_, ts)) = open {
            self.close_and_delete_stream(profile, workspace, channel, &ts)
                .await;
        }
    }

    async fn clear_stream_for_run(
        &self,
        profile: &ProfileId,
        workspace: &str,
        channel: &str,
        thread_ts: &str,
        run: &RunId,
    ) {
        let Some(slot) = self.existing_stream_slot(channel, thread_ts) else {
            return;
        };
        let mut guard = slot.lock().await;
        let ts = if guard.open.as_ref().is_some_and(|(r, _)| r == run) {
            guard.closed_for = Some(run.clone());
            guard.open.take().map(|(_, ts)| ts)
        } else {
            if guard.open.is_none() {
                guard.closed_for = Some(run.clone());
            }
            None
        };
        if let Some(ts) = ts {
            self.close_and_delete_stream(profile, workspace, channel, &ts)
                .await;
        }
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
            self.clear_stream_for_run(profile, workspace, channel, thread_ts, run)
                .await;
            return;
        }
        let Some(token) = self.creds.bot_token(profile, workspace).await else {
            return;
        };
        let slot = self.stream_slot(channel, thread_ts);
        let mut guard = slot.lock().await;
        if guard.closed_for.as_ref() == Some(run) {
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
                    .call("chat.appendStream", &token, Body::Json(&body))
                    .await
                {
                    Raw::Ok(_) => {}
                    _ => {
                        guard.open = None;
                        let del = json!({"channel": channel, "ts": ts});
                        let _ = self
                            .creds
                            .call("chat.delete", &token, Body::Json(&del))
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
                    .call("chat.startStream", &token, Body::Json(&body))
                    .await
                    && let Some(ts) = val["ts"].as_str()
                {
                    guard.open = Some((run.clone(), ts.to_owned()));
                }
            }
        }
    }

    /// Slack Agent 화면의 중단 버튼(`agent_session_stopped`): 확인 카드를 올린다.
    pub async fn on_stop_requested(
        &self,
        install: &Installation,
        snapshot: Option<&Snapshot>,
        channel: &str,
        thread_ts: &str,
        user: &str,
    ) {
        let Some(active) = self.active_by_thread(channel, thread_ts) else {
            return;
        };
        if active.info.profile != install.profile {
            return;
        }
        if !user.is_empty() {
            let Some(actor) = identity::slack(&install.alias, user) else {
                return;
            };
            if !can_stop(snapshot, &actor, &active.info.principal) {
                self.deny_stop(&active, user).await;
                return;
            }
        }
        let (step, doing) = {
            let mut st = active.st.lock().unwrap_or_else(|e| e.into_inner());
            if st.stop_asked || st.stop_confirmed {
                return;
            }
            st.stop_asked = true;
            (st.step, st.doing.clone())
        };
        let (blocks, fallback) = stop_card_blocks(
            step,
            &doing,
            active.started.elapsed().as_secs(),
            false,
            Some(&active.info.run.0),
        );
        match self
            .post_blocks(
                &active.info.profile,
                &active.info.workspace,
                channel,
                thread_ts,
                blocks,
                &fallback,
            )
            .await
        {
            Some(ts) => {
                active.st.lock().unwrap_or_else(|e| e.into_inner()).stop_ts = ts;
            }
            None => {
                active
                    .st
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .stop_asked = false;
            }
        }
    }

    /// 처리 중인 스레드에서 "중단해줘"/"멈춰" 멘션. 진행 중 작업이 있으면 가로채서 중단하고 `true`.
    pub async fn on_stop_mention(
        &self,
        install: &Installation,
        snapshot: Option<&Snapshot>,
        channel: &str,
        thread_ts: &str,
        user: &str,
    ) -> bool {
        let Some(active) = self.active_by_thread(channel, thread_ts) else {
            return false;
        };
        if active.info.profile != install.profile {
            return false;
        }
        let Some(actor) = identity::slack(&install.alias, user) else {
            return true;
        };
        if !can_stop(snapshot, &actor, &active.info.principal) {
            self.deny_stop(&active, user).await;
            return true;
        }
        self.confirm_stop(&active, actor).await;
        true
    }

    /// 중단 확인 카드의 [여기서 중단] / [계속 진행] 버튼 클릭.
    pub async fn on_stop_button(
        &self,
        install: &Installation,
        snapshot: Option<&Snapshot>,
        button: &Button,
        confirm: bool,
    ) {
        let active = self.active_by_run(&button.value);
        let state = active.as_ref().map(|r| {
            let st = r.st.lock().unwrap_or_else(|e| e.into_inner());
            (st.stop_confirmed, st.stop_asked, st.stop_ts.clone())
        });
        match (&active, state) {
            (Some(_), Some((true, _, _))) => {}
            (Some(active), Some((false, true, stop_ts)))
                if active.info.profile == install.profile
                    && (stop_ts.is_empty() || stop_ts == button.message_ts) =>
            {
                let Some(actor) = identity::slack(&install.alias, &button.user) else {
                    return;
                };
                if !can_stop(snapshot, &actor, &active.info.principal) {
                    self.deny_stop(active, &button.user).await;
                    return;
                }
                if confirm {
                    self.confirm_stop(active, actor).await;
                } else {
                    active
                        .st
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .stop_asked = false;
                    self.settle_stop_notice(active, STOP_RESUMED).await;
                }
            }
            _ => {
                self.update_plain(
                    &install.profile,
                    &install.alias,
                    &button.channel,
                    &button.message_ts,
                    STOP_GONE,
                )
                .await;
            }
        }
    }

    async fn confirm_stop(&self, active: &ActiveRun, actor: PrincipalId) {
        {
            let mut st = active.st.lock().unwrap_or_else(|e| e.into_inner());
            if st.stop_confirmed {
                return;
            }
            st.stop_confirmed = true;
            st.stopped_snapshot = Some((
                st.step,
                st.doing.clone(),
                active.started.elapsed().as_secs(),
            ));
        }
        let res = self
            .admin
            .cancel(
                active.info.profile.clone(),
                active.info.run.clone(),
                actor,
                "slack_user_stop".into(),
            )
            .await;
        if let Err(error) = res {
            tracing::warn!(?error, run = %active.info.run.0, "Slack 중단 취소 요청 실패");
        }
        self.settle_stop_notice(active, STOP_ACK).await;
    }

    async fn settle_stop_notice(&self, active: &ActiveRun, text: &str) {
        let ts = std::mem::take(
            &mut active
                .st
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .stop_ts,
        );
        if !ts.is_empty() {
            self.update_plain(
                &active.info.profile,
                &active.info.workspace,
                &active.info.channel,
                &ts,
                text,
            )
            .await;
        }
    }

    async fn deny_stop(&self, active: &ActiveRun, user: &str) {
        if user.is_empty() {
            return;
        }
        let Some(token) = self
            .creds
            .bot_token(&active.info.profile, &active.info.workspace)
            .await
        else {
            return;
        };
        let text = format!(
            "중단은 요청한 본인(<@{}>)이나 관리자만 할 수 있어요.",
            active.info.user
        );
        let mut body = json!({"channel": active.info.channel, "user": user, "text": text});
        if !active.info.thread_ts.is_empty() {
            body["thread_ts"] = json!(active.info.thread_ts);
        }
        let _ = self
            .creds
            .call("chat.postEphemeral", &token, Body::Json(&body))
            .await;
    }

    async fn react(&self, profile: &ProfileId, workspace: &str, channel: &str, ts: &str, name: &str) {
        let Some(token) = self.creds.bot_token(profile, workspace).await else {
            return;
        };
        let body = json!({"channel": channel, "timestamp": ts, "name": name});
        let _ = self
            .creds
            .call("reactions.add", &token, Body::Json(&body))
            .await;
    }

    async fn unreact(
        &self,
        profile: &ProfileId,
        workspace: &str,
        channel: &str,
        ts: &str,
        name: &str,
    ) {
        let Some(token) = self.creds.bot_token(profile, workspace).await else {
            return;
        };
        let body = json!({"channel": channel, "timestamp": ts, "name": name});
        let _ = self
            .creds
            .call("reactions.remove", &token, Body::Json(&body))
            .await;
    }

    async fn finish_reaction(
        &self,
        profile: &ProfileId,
        workspace: &str,
        channel: &str,
        ts: &str,
        final_emoji: &str,
    ) {
        tokio::join!(
            self.unreact(profile, workspace, channel, ts, EYES),
            self.react(profile, workspace, channel, ts, final_emoji),
        );
    }

    async fn post_blocks(
        &self,
        profile: &ProfileId,
        workspace: &str,
        channel: &str,
        thread_ts: &str,
        blocks: Vec<Value>,
        text: &str,
    ) -> Option<String> {
        let token = self.creds.bot_token(profile, workspace).await?;
        let mut body = json!({
            "channel": channel,
            "blocks": blocks,
            "text": text,
            "unfurl_links": false,
            "unfurl_media": false
        });
        if !thread_ts.is_empty() {
            body["thread_ts"] = json!(thread_ts);
        }
        match self
            .creds
            .call("chat.postMessage", &token, Body::Json(&body))
            .await
        {
            Raw::Ok(val) => val["ts"].as_str().map(str::to_owned),
            _ => None,
        }
    }

    async fn update_plain(
        &self,
        profile: &ProfileId,
        workspace: &str,
        channel: &str,
        ts: &str,
        text: &str,
    ) {
        if ts.is_empty() {
            return;
        }
        let Some(token) = self.creds.bot_token(profile, workspace).await else {
            return;
        };
        let body = json!({"channel": channel, "ts": ts, "text": text, "blocks": []});
        let _ = self
            .creds
            .call("chat.update", &token, Body::Json(&body))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_phrase_matches_only_exact_commands() {
        assert!(is_stop_phrase("중단해줘"));
        assert!(is_stop_phrase("중단 해 주세요"));
        assert!(is_stop_phrase("멈춰!"));
        assert!(is_stop_phrase("  멈춰줘. "));
        assert!(is_stop_phrase("중단해줘 자동 발송 · 테스트"));
        assert!(!is_stop_phrase("이거 중단해줘"));
        assert!(!is_stop_phrase("멈춰있는 서버 원인 알려줘"));
        assert!(!is_stop_phrase("중단하지 마"));
        assert!(!is_stop_phrase(""));
    }

    #[test]
    fn stepper_marks_done_current_and_stopped() {
        assert_eq!(
            stepper(2, false),
            "🟩 요청 확인  ›  🟩 맥락 파악  ›  🟨 답 작성  ›  ⬜ 전송"
        );
        assert_eq!(
            stepper(2, true),
            "🟩 요청 확인  ›  🟩 맥락 파악  ›  🟥 답 작성  ›  ⬜ 전송"
        );
    }

    #[test]
    fn describe_progress_prioritizes_cancel_approval_tool_and_agent() {
        let ready = RunProgress {
            state: RunState::Ready,
            cancel_requested: false,
            active_agent: None,
            active_tool: None,
            waiting_approval: false,
        };
        assert_eq!(describe_progress(&ready, 0).1, None);
        let base = RunProgress {
            state: RunState::Leased,
            ..ready
        };
        assert_eq!(
            describe_progress(&base, 3).1.as_deref(),
            Some("답을 만들고 있어요")
        );
        assert_eq!(
            describe_progress(&base, 20).1.as_deref(),
            Some("깊이 생각하고 있어요")
        );
        let with_agent = RunProgress {
            active_agent: Some("main/proxy".into()),
            ..base.clone()
        };
        assert_eq!(
            describe_progress(&with_agent, 5).1.as_deref(),
            Some("proxy 역할에서 작업하고 있어요")
        );
        let with_tool = RunProgress {
            active_tool: Some("proxy-stats.usage-chart".into()),
            ..with_agent.clone()
        };
        assert_eq!(
            describe_progress(&with_tool, 5).1.as_deref(),
            Some("도구 실행 중: proxy-stats.usage-chart")
        );
        let waiting = RunProgress {
            waiting_approval: true,
            ..with_tool.clone()
        };
        assert!(
            describe_progress(&waiting, 5)
                .1
                .as_deref()
                .is_some_and(|s| s.contains("승인을 기다리고"))
        );
        let cancelling = RunProgress {
            cancel_requested: true,
            ..waiting
        };
        assert_eq!(
            describe_progress(&cancelling, 5).1.as_deref(),
            Some("중단하고 있어요")
        );
    }

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
