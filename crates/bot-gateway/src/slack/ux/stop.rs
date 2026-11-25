//! Slack 스레드 중단 처리(Agent 중단 버튼 · 멘션 중단 · 확인 버튼)와 반응/메시지 보조 호출.
use super::{
    ActiveRun, EYES, SlackUx,
    cards::{STOP_ACK, STOP_GONE, STOP_RESUMED, can_stop, stop_card_blocks},
};
use crate::{
    identity,
    slack::{
        api::{Body, Raw},
        frame::Button,
        socket::Installation,
    },
};
use bot_core::{
    id::{PrincipalId, ProfileId},
    profile::Snapshot,
};
use serde_json::{Value, json};

impl SlackUx {
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
