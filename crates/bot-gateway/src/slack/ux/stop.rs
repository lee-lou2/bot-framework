//! Slack thread stop handling (agent stop button, stop mentions, confirmation cards) and reaction/message helpers.
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

/// Outcome of handling a stop mention in a thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopMention {
    /// No active run in this thread; fall through to normal message intake.
    Pass,
    /// Stop was handled or definitively ignored (unauthorized or stale); safe to ack.
    Handled,
    /// Transient `Unavailable` error; do not ack so Slack redelivers.
    Retry,
}

/// Clears `stop_in_flight` on drop (including panic/cancellation) to serialize concurrent `admin.cancel` calls.
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
    /// Handles `agent_session_stopped` by posting a confirmation card; returns whether to ack (`false` only on `Unavailable`).
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
        // Fail closed on missing or malformed actor ID (never bypass authorization).
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

    /// Intercepts a stop mention in a thread with an active run and cancels it directly.
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

    /// Handles the confirmation card's Stop/Continue buttons; returns whether to ack (`false` only on `Unavailable`).
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

    /// Commits cancellation in storage before marking confirmed and posting `STOP_ACK`.
    /// Returns `false` on `Error::Unavailable` so Slack does not ack and redelivers.
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
                tracing::warn!(run = %active.info.run.0, "transient failure cancelling Slack run");
                false
            }
            Err(error) => {
                tracing::warn!(?error, run = %active.info.run.0, "failed to cancel Slack run");
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
            "Only the original requester (<@{}>) or an admin can stop this task.",
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
