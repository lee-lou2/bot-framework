//! Slack 스레드 중단 처리(Agent 중단 버튼 · 멘션 중단 · 확인 버튼)와 반응/메시지 보조 호출.
use super::{
    ActiveRun, EYES, RunUxState, SlackUx,
    cards::{STOP_ACK, STOP_GONE, STOP_RESUMED, can_stop, stop_card_blocks},
};
use crate::{
    identity,
    ingress::Ingress,
    slack::{
        api::{Body, Raw},
        frame::Button,
        socket::Installation,
    },
};
use bot_core::{
    id::{PrincipalId, ProfileId},
    port::Error,
    profile::Snapshot,
};
use serde_json::{Value, json};
use std::sync::Mutex;

/// 스레드 중단 멘션("중단해줘" 등) 처리 결과.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopMention {
    /// 이 스레드에 진행 중인 작업이 없어 일반 메시지로 넘긴다.
    Pass,
    /// 중단 요청을 처리했거나 권한 없음·구판으로 무시 확정했다(ack 가능).
    Handled,
    /// 일시 장애(`Unavailable`)로 취소를 확정하지 못해 ack하지 않고 재전달에 맡긴다.
    Retry,
}

/// 동시 클릭이 중복 `admin.cancel`을 보내지 못하게 막고, 취소·패닉 시에도 진행 중 표지를 해제하는 가드.
struct InFlightGuard<'a>(&'a Mutex<RunUxState>);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stop_in_flight = false;
    }
}

impl SlackUx {
    /// Slack Agent 화면의 중단 버튼(`agent_session_stopped`): 확인 카드를 올린다.
    /// 돌려주는 값은 «ack해도 되는가»다(`Unavailable`만 `false`).
    pub async fn on_stop_requested(
        &self,
        ingress: &Ingress,
        install: &Installation,
        snapshot: Option<&Snapshot>,
        channel: &str,
        thread_ts: &str,
        user: &str,
    ) -> bool {
        let Some(active) = self.active_by_thread(channel, thread_ts) else {
            return true;
        };
        if active.info.profile != install.profile {
            return true;
        }
        // 행위자 ID가 비었거나 모양이 틀리면 닫힌 쪽으로 무시한다(인가 생략 금지).
        let Some(actor) = (!user.is_empty())
            .then(|| identity::slack(&install.alias, user))
            .flatten()
        else {
            return true;
        };
        match ingress.verify_active().await {
            Ok(()) => {}
            Err(Error::Unavailable) => return false,
            Err(_) => return true,
        }
        if !can_stop(snapshot, &actor, &active.info.principal) {
            self.deny_stop(&active, user).await;
            return true;
        }
        let (step, doing) = {
            let mut st = active.st.lock().unwrap_or_else(|e| e.into_inner());
            if st.stop_asked || st.stop_in_flight || st.stop_confirmed {
                return true;
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
        true
    }

    /// 처리 중인 스레드에서 "중단해줘"/"멈춰" 멘션. 진행 중 작업이 있으면 가로채서 중단한다.
    pub async fn on_stop_mention(
        &self,
        ingress: &Ingress,
        install: &Installation,
        snapshot: Option<&Snapshot>,
        channel: &str,
        thread_ts: &str,
        user: &str,
    ) -> StopMention {
        let Some(active) = self.active_by_thread(channel, thread_ts) else {
            return StopMention::Pass;
        };
        if active.info.profile != install.profile {
            return StopMention::Pass;
        }
        let Some(actor) = (!user.is_empty())
            .then(|| identity::slack(&install.alias, user))
            .flatten()
        else {
            return StopMention::Handled;
        };
        match ingress.verify_active().await {
            Ok(()) => {}
            Err(Error::Unavailable) => return StopMention::Retry,
            Err(_) => return StopMention::Handled,
        }
        if !can_stop(snapshot, &actor, &active.info.principal) {
            self.deny_stop(&active, user).await;
            return StopMention::Handled;
        }
        if self.confirm_stop(&active, actor).await {
            StopMention::Handled
        } else {
            StopMention::Retry
        }
    }

    /// 중단 확인 카드의 [여기서 중단] / [계속 진행] 버튼 클릭.
    /// 돌려주는 값은 «ack해도 되는가»다(`Unavailable`만 `false`).
    pub async fn on_stop_button(
        &self,
        ingress: &Ingress,
        install: &Installation,
        snapshot: Option<&Snapshot>,
        button: &Button,
        confirm: bool,
    ) -> bool {
        match ingress.verify_active().await {
            Ok(()) => {}
            Err(Error::Unavailable) => return false,
            Err(_) => return true,
        }
        let active = self.active_by_run(&button.value);
        let state = active.as_ref().map(|r| {
            let st = r.st.lock().unwrap_or_else(|e| e.into_inner());
            (
                st.stop_confirmed,
                st.stop_in_flight,
                st.stop_asked,
                st.stop_ts.clone(),
            )
        });
        match (&active, state) {
            (Some(_), Some((true, _, _, _))) | (Some(_), Some((_, true, _, _))) => true,
            (Some(active), Some((false, false, true, stop_ts)))
                if active.info.profile == install.profile
                    && (stop_ts.is_empty() || stop_ts == button.message_ts) =>
            {
                let Some(actor) = (!button.user.is_empty())
                    .then(|| identity::slack(&install.alias, &button.user))
                    .flatten()
                else {
                    return true;
                };
                if !can_stop(snapshot, &actor, &active.info.principal) {
                    self.deny_stop(active, &button.user).await;
                    return true;
                }
                if confirm {
                    self.confirm_stop(active, actor).await
                } else {
                    let reset = {
                        let mut st = active.st.lock().unwrap_or_else(|e| e.into_inner());
                        if !st.stop_confirmed && !st.stop_in_flight {
                            st.stop_asked = false;
                            true
                        } else {
                            false
                        }
                    };
                    if reset {
                        self.settle_stop_notice(active, STOP_RESUMED).await;
                    }
                    true
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
                true
            }
        }
    }

    /// 저장소에 취소를 먼저 커밋한 뒤에만 확정 상태와 `STOP_ACK`를 반영한다.
    /// `Err(Error::Unavailable)`이면 `false`를 돌려줘 Slack이 ack하지 않고 재전달하게 한다.
    async fn confirm_stop(&self, active: &ActiveRun, actor: PrincipalId) -> bool {
        let snapshot = {
            let mut st = active.st.lock().unwrap_or_else(|e| e.into_inner());
            if st.stop_confirmed || st.stop_in_flight {
                return true;
            }
            st.stop_in_flight = true;
            (
                st.step,
                st.doing.clone(),
                active.started.elapsed().as_secs(),
            )
        };
        let _guard = InFlightGuard(&active.st);
        let res = self
            .admin
            .cancel(
                active.info.profile.clone(),
                active.info.run.clone(),
                actor,
                "slack_user_stop".into(),
            )
            .await;
        match res {
            Ok(()) => {
                {
                    let mut st = active.st.lock().unwrap_or_else(|e| e.into_inner());
                    st.stop_confirmed = true;
                    st.stopped_snapshot = Some(snapshot);
                }
                drop(_guard);
                self.settle_stop_notice(active, STOP_ACK).await;
                true
            }
            Err(Error::Unavailable) => {
                tracing::warn!(run = %active.info.run.0, "Slack 중단 취소 요청 일시 실패");
                false
            }
            Err(error) => {
                tracing::warn!(?error, run = %active.info.run.0, "Slack 중단 취소 요청 실패");
                true
            }
        }
    }

    pub(super) async fn settle_stop_notice(&self, active: &ActiveRun, text: &str) {
        let ts = std::mem::take(&mut active.st.lock().unwrap_or_else(|e| e.into_inner()).stop_ts);
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

    pub(super) async fn react(
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
            .call_passive("reactions.add", &token, Body::Json(&body))
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
            .call_passive("reactions.remove", &token, Body::Json(&body))
            .await;
    }

    pub(super) async fn finish_reaction(
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

    pub(super) async fn post_blocks(
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
