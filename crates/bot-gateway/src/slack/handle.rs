//! Slack frame dispatch for authenticated installations: messages to `Ingress`, approval buttons to `Broker::decide`.
//!
//! Never falls back across profiles: events whose team or route does not match this installation are ignored.
use super::{
    SlackGateway,
    api::Body,
    blocks::{APPROVE, DENY, resolved_blocks},
    cards::is_the_card,
    events::SlackAdmission,
    frame::{Button, Frame, Message, StopRequested},
    socket::Installation,
    ux::{STOP_CONFIRM, STOP_CONTINUE, StopMention, is_stop_phrase},
};
use crate::{
    entrance::Real,
    identity,
    inbound::{Event, valid_request_id},
    observe,
    reply::parse_card,
};
use bot_core::{
    id::ApprovalId, ledger::ApprovalState, port::Error, slack::SourceKind, state::ConversationKind,
    trace::now_nano,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// Strips only `<@bot_user>` and `<@bot_user|name>` mentions, leaving other users and prefix-matching IDs intact.
pub fn strip_mention(text: &str, bot_user: &str) -> String {
    let open = format!("<@{bot_user}");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(&open) {
        let after = &rest[at + open.len()..];
        let tail = after.strip_prefix('>').or_else(|| {
            after
                .strip_prefix('|')
                .and_then(|named| named.find('>').map(|end| &named[end + 1..]))
        });
        match tail {
            Some(tail) => {
                out.push_str(&rest[..at]);
                rest = tail;
            }
            None => {
                out.push_str(&rest[..at + open.len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

pub fn mentions_bot(text: &str, bot_user: &str) -> bool {
    let open = format!("<@{bot_user}");
    let mut rest = text;
    while let Some(at) = rest.find(&open) {
        let after = &rest[at + open.len()..];
        if after.starts_with('>')
            || after
                .strip_prefix('|')
                .is_some_and(|named| named.contains('>'))
        {
            return true;
        }
        rest = after;
    }
    false
}

const RETRY_NOTICE: &str = "Could not process right now — please try clicking again shortly.";

enum Click {
    Ignored,
    Done(&'static str),
    Retry,
}

fn approval_id(nonce: &str) -> ApprovalId {
    ApprovalId(nonce.to_owned())
}

fn status_line(state: ApprovalState) -> Option<&'static str> {
    match state {
        ApprovalState::Pending => None,
        ApprovalState::Approved | ApprovalState::Consumed => Some("✅ Approved"),
        ApprovalState::Denied => Some("⛔ Denied"),
        ApprovalState::Expired => Some("⌛ Expired"),
        ApprovalState::Superseded => Some("↻ Conditions changed; new approval required"),
        ApprovalState::Revoked => Some("⛔ Revoked"),
    }
}

impl SlackGateway {
    /// Handles a frame and returns whether the Socket Mode envelope may be acknowledged.
    pub async fn on_frame(&self, install: &Installation, frame: Frame) -> bool {
        self.on_frame_with_stop(install, frame, &self.shutdown)
            .await
    }

    pub(super) async fn on_frame_with_stop(
        &self,
        install: &Installation,
        frame: Frame,
        stop: &CancellationToken,
    ) -> bool {
        match frame {
            Frame::Message(message) if !message.direct => {
                self.on_channel_message(install, message.into(), stop).await
            }
            Frame::Message(message) => self.on_message(install, message, stop).await,
            Frame::Post(post) => self.on_post(install, post, stop).await,
            Frame::Reaction(reaction) => self.on_reaction(install, reaction).await,
            Frame::Button(button) => self.on_button(install, button).await,
            Frame::StopRequested(req) => self.on_stop_requested(install, req).await,
            _ => true,
        }
    }

    async fn on_stop_requested(&self, install: &Installation, req: StopRequested) -> bool {
        if req.team != install.team {
            return true;
        }
        let Some(ux) = &self.ux else { return true };
        let Ok(routes) = self.ingress.routes() else {
            return false;
        };
        let snapshot = routes.snapshot(&install.profile);
        ux.on_stop_requested(
            &self.ingress,
            install,
            snapshot.map(|v| &**v),
            &req.channel,
            &req.thread_ts,
            &req.user,
        )
        .await
    }

    async fn on_message(
        &self,
        install: &Installation,
        message: Message,
        stop: &CancellationToken,
    ) -> bool {
        if message.team != install.team {
            return true;
        }
        let real = Real::SlackDm {
            team: message.team.clone(),
        };
        let Ok(routes) = self.ingress.routes() else {
            return false;
        };
        match routes.find(&real) {
            Some(route) if route.snapshot.id == install.profile => {}
            _ => return true,
        }
        let Some(principal) = identity::slack(&install.alias, &message.user) else {
            return true;
        };
        let request = format!("{}.{}", message.channel, message.ts);
        if !valid_request_id(&request) {
            return true;
        }
        let stripped = strip_mention(&message.text, &install.bot_user);
        let thread_ts = message
            .thread_ts
            .clone()
            .unwrap_or_else(|| message.ts.clone());
        if is_stop_phrase(&stripped) {
            match self
                .try_stop_mention(
                    install,
                    &routes,
                    &message.channel,
                    &thread_ts,
                    &message.user,
                )
                .await
            {
                StopMention::Pass => {}
                StopMention::Handled => return true,
                StopMention::Retry => return false,
            }
        }
        self.admit_slack_message(
            install,
            &routes,
            SlackAdmission {
                real,
                principal,
                request,
                kind: ConversationKind::Direct,
                source_kind: SourceKind::Message,
                team: message.team,
                channel: message.channel,
                user: message.user,
                ts: message.ts,
                raw_thread_ts: message.thread_ts,
                thread_ts,
                files: message.files,
                event_id: message.event_id,
                body: json!({"text": stripped}),
                event: Event::Message,
                track_ux: true,
            },
            stop,
        )
        .await
    }

    async fn on_button(&self, install: &Installation, button: Button) -> bool {
        if matches!(button.action_id.as_str(), STOP_CONFIRM | STOP_CONTINUE) {
            if button.team == install.team
                && let Some(ux) = &self.ux
            {
                let Ok(routes) = self.ingress.routes() else {
                    return false;
                };
                let snapshot = routes.snapshot(&install.profile);
                return ux
                    .on_stop_button(
                        &self.ingress,
                        install,
                        snapshot.map(|v| &**v),
                        &button,
                        button.action_id == STOP_CONFIRM,
                    )
                    .await;
            }
            return true;
        }
        let started = now_nano();
        let principal = identity::slack(&install.alias, &button.user);
        let click = self.decide_click(install, &button).await;
        let outcome = match &click {
            Click::Ignored => None,
            Click::Done(outcome) => Some(*outcome),
            Click::Retry => Some("unavailable"),
        };
        if let Some(outcome) = outcome {
            observe::record_approval(
                &**self.ingress.tracer(),
                &install.profile,
                principal.as_ref().map_or("", |p| p.0.as_str()),
                outcome,
                started,
            );
        }
        !matches!(click, Click::Retry)
    }

    async fn decide_click(&self, install: &Installation, button: &Button) -> Click {
        if button.team != install.team {
            return Click::Ignored;
        }
        let allow = match button.action_id.as_str() {
            APPROVE => true,
            DENY => false,
            _ => return Click::Ignored,
        };
        let nonce = button.value.clone();
        if nonce.is_empty() || nonce.len() > 200 || !nonce.bytes().all(|b| b.is_ascii_graphic()) {
            return Click::Ignored;
        }
        if self.ingress.verify_active().await.is_err() {
            self.notice(install, button, RETRY_NOTICE).await;
            return Click::Retry;
        }
        let Ok(routes) = self.ingress.routes() else {
            return Click::Retry;
        };
        let Some(principal) = identity::slack(&install.alias, &button.user) else {
            return Click::Ignored;
        };
        let approval = ApprovalId(nonce.clone());
        let stored = match self.ledger.approval(approval.clone()).await {
            Ok(stored) => Some(stored),
            Err(Error::Unavailable | Error::Stale) => {
                self.notice(install, button, RETRY_NOTICE).await;
                return Click::Retry;
            }
            Err(_) => None,
        };
        // Owner-DM cards with a delivery receipt accept clicks only from that exact DM message (no DM input binding needed).
        let snapshot = match stored.as_ref().and_then(|stored| stored.delivery.as_ref()) {
            Some(receipt) => {
                let Some(snapshot) = routes.snapshot(&install.profile).cloned() else {
                    return Click::Ignored;
                };
                if !is_the_card(receipt, install, &snapshot, button) {
                    self.notice(install, button, "Cannot approve using this card.")
                        .await;
                    return Click::Done("wrong_card");
                }
                snapshot
            }
            None => {
                let real = if button.channel.starts_with('D') {
                    Real::SlackDm {
                        team: button.team.clone(),
                    }
                } else {
                    Real::SlackChannel {
                        team: button.team.clone(),
                        channel: button.channel.clone(),
                    }
                };
                match routes.find(&real) {
                    Some(route) if route.snapshot.id == install.profile => route.snapshot.clone(),
                    _ => {
                        let state = stored
                            .as_ref()
                            .filter(|stored| stored.approval.attempt.profile == install.profile)
                            .map(|stored| stored.state);
                        let member = routes
                            .snapshot(&install.profile)
                            .is_some_and(|snapshot| snapshot.members.contains_key(&principal));
                        drop(routes);
                        return match state {
                            Some(state) if button.channel.starts_with('D') && member => {
                                self.unopened_card(install, button, state).await
                            }
                            _ => Click::Ignored,
                        };
                    }
                }
            }
        };
        drop(routes);
        if !snapshot.members.contains_key(&principal) {
            self.notice(
                install,
                button,
                "Only registered users can act on this approval.",
            )
            .await;
            return Click::Done("unregistered");
        }
        if !stored
            .as_ref()
            .is_some_and(|stored| stored.approval.attempt.profile == install.profile)
        {
            self.notice(install, button, "Unknown approval card.").await;
            return Click::Done("unknown_card");
        }
        let outcome = match self.broker.decide(approval, principal, allow).await {
            Ok(()) if allow => "approved",
            Ok(()) => "denied",
            Err(Error::Denied) => {
                self.notice(
                    install,
                    button,
                    "Only the designated approver can act on this approval.",
                )
                .await;
                return Click::Done("forbidden");
            }
            Err(Error::Stale | Error::Missing) => {
                self.notice(install, button, "This card is no longer valid.")
                    .await;
                "stale"
            }
            // Conflicting decision already committed: re-read the ledger and settle only if terminal.
            Err(Error::Conflict) => match self.ledger.approval(approval_id(&nonce)).await {
                Ok(stored) if status_line(stored.state).is_some() => {
                    self.notice(
                        install,
                        button,
                        "A different decision has already been recorded for this card.",
                    )
                    .await;
                    "conflict"
                }
                _ => {
                    self.notice(install, button, RETRY_NOTICE).await;
                    return Click::Retry;
                }
            },
            Err(_) => {
                self.notice(install, button, RETRY_NOTICE).await;
                return Click::Retry;
            }
        };
        self.refresh_card(install, button, &nonce).await;
        Click::Done(outcome)
    }

    async fn unopened_card(
        &self,
        install: &Installation,
        button: &Button,
        state: ApprovalState,
    ) -> Click {
        if state == ApprovalState::Pending {
            self.notice(
                install,
                button,
                "This card is not open yet — please try again shortly.",
            )
            .await;
            Click::Done("not_open")
        } else {
            self.notice(install, button, "This request is already closed.")
                .await;
            Click::Done("closed")
        }
    }

    async fn refresh_card(&self, install: &Installation, button: &Button, nonce: &str) {
        let Ok(stored) = self.ledger.approval(ApprovalId(nonce.to_owned())).await else {
            return;
        };
        let Some(status) = status_line(stored.state) else {
            return;
        };
        if button.message_ts.is_empty() {
            return;
        }
        let card = serde_json::from_str::<Value>(&stored.card.0)
            .ok()
            .and_then(|value| parse_card(&value).ok());
        let blocks = match card {
            Some(card) => resolved_blocks(&card, status),
            None => vec![
                json!({"type": "context", "elements": [{"type": "plain_text", "text": status}]}),
            ],
        };
        let Some(token) = self.creds.bot_token(&install.profile, &install.alias).await else {
            return;
        };
        let body = json!({"channel": button.channel, "ts": button.message_ts, "blocks": blocks, "text": status});
        let result = self
            .creds
            .call("chat.update", &token, Body::Json(&body))
            .await;
        if !matches!(result, super::api::Raw::Ok(_)) {
            tracing::warn!("failed to refresh approval card (decision already recorded)");
        }
    }

    async fn notice(&self, install: &Installation, button: &Button, text: &str) {
        let Some(token) = self.creds.bot_token(&install.profile, &install.alias).await else {
            return;
        };
        let body = json!({"channel": button.channel, "user": button.user, "text": text});
        self.creds
            .call("chat.postEphemeral", &token, Body::Json(&body))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_the_bots_own_mention() {
        assert_eq!(strip_mention("<@UB> hello", "UB"), "hello");
        assert_eq!(strip_mention("hello <@UB|bot> world", "UB"), "hello  world");
        assert_eq!(strip_mention("<@UB><@UB> two", "UB"), "two");
        assert_eq!(strip_mention("<@UB> <@U1> please", "UB"), "<@U1> please");
        assert_eq!(strip_mention("<@UB1> hello", "UB"), "<@UB1> hello");
        assert_eq!(strip_mention("<@UB unclosed", "UB"), "<@UB unclosed");
        assert_eq!(strip_mention("no mention", "UB"), "no mention");
        assert_eq!(strip_mention("   ", "UB"), "");
    }

    #[test]
    fn approval_states_map_to_one_status_line() {
        assert_eq!(status_line(ApprovalState::Pending), None);
        assert_eq!(status_line(ApprovalState::Approved), Some("✅ Approved"));
        assert_eq!(status_line(ApprovalState::Consumed), Some("✅ Approved"));
        for state in [ApprovalState::Denied, ApprovalState::Revoked] {
            assert_eq!(
                status_line(state),
                Some(if state == ApprovalState::Denied {
                    "⛔ Denied"
                } else {
                    "⛔ Revoked"
                })
            );
        }
        assert!(
            status_line(ApprovalState::Expired)
                .unwrap()
                .contains("Expired")
        );
        assert!(
            status_line(ApprovalState::Superseded)
                .unwrap()
                .contains("new approval")
        );
    }
}
