//! Telegram update dispatch: private DM and group mention/reply intake plus DM approval callbacks.
//!
//! Never falls back across profiles: updates whose bot or route does not match this installation are ignored.
use super::{
    Installation, TelegramGateway,
    api::Raw,
    cards::is_the_card,
    creds::BotCheck,
    parse::{
        extract_attachments, forum_topic_id, mentions_bot, negative_i64, positive_u64,
        replies_to_bot, reply_context_prefix, strip_bot_mention,
    },
    render::{parse_callback, resolved_card_text, status_line},
    ux::TrackedRun,
};
use crate::{
    entrance::Real,
    identity,
    inbound::{Event, Inbound, valid_request_id},
    ingress::Admission,
    observe,
    reply::parse_card,
};
use bot_core::{
    id::{ApprovalId, ProfileId, RequestId},
    ledger::ApprovalState,
    port::Error,
    state::{Conversation, ConversationKind},
    trace::now_nano,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const RETRY_NOTICE: &str = "Could not process right now — please try clicking again shortly.";

impl TelegramGateway {
    /// Processes one Telegram Update object and returns whether it is settled (safe to advance `offset`).
    pub async fn on_update(
        &self,
        install: &Installation,
        update: &Value,
        stop: &CancellationToken,
    ) -> bool {
        if let Some(callback) = update.get("callback_query") {
            return self.on_callback(install, callback).await;
        }
        if let Some(message) = update.get("message") {
            return self.on_message(install, message, stop).await;
        }
        true
    }

    async fn on_message(
        &self,
        install: &Installation,
        message: &Value,
        stop: &CancellationToken,
    ) -> bool {
        let Some(chat) = message.get("chat") else {
            return true;
        };
        let Some(chat_type) = chat["type"].as_str() else {
            return true;
        };
        let is_group = match chat_type {
            "private" => false,
            "group" | "supergroup" => true,
            _ => return true,
        };
        let Some(from) = message.get("from") else {
            return true;
        };
        if from["is_bot"].as_bool() == Some(true) {
            return true;
        }
        let (Some(user_id), Some(message_id)) = (
            positive_u64(&from["id"]),
            positive_u64(&message["message_id"]),
        ) else {
            return true;
        };
        let chat_id: i64 = if is_group {
            let Some(id) = negative_i64(&chat["id"]) else {
                return true;
            };
            id
        } else {
            let Some(id) = positive_u64(&chat["id"]).filter(|id| *id == user_id) else {
                return true;
            };
            let Ok(signed) = i64::try_from(id) else {
                return true;
            };
            signed
        };

        let (raw_text, entities) = if let Some(text) = message["text"].as_str() {
            (Some(text), message.get("entities"))
        } else if let Some(caption) = message["caption"].as_str() {
            (Some(caption), message.get("caption_entities"))
        } else {
            (None, None)
        };

        if is_group {
            let mentioned = raw_text.is_some_and(|text| {
                install
                    .username
                    .as_deref()
                    .is_some_and(|username| mentions_bot(text, entities, username))
            });
            let replied = replies_to_bot(message, install.bot_id);
            if !mentioned && !replied {
                return true;
            }
        }

        let real = if is_group {
            Real::TelegramGroups {
                bot_id: install.bot_id,
            }
        } else {
            Real::TelegramDm {
                bot_id: install.bot_id,
            }
        };
        let Ok(routes) = self.ingress.routes() else {
            return false;
        };
        match routes.find(&real) {
            Some(route) if route.snapshot.id == install.profile => {}
            _ => return true,
        }
        let Some(principal) = identity::telegram(user_id) else {
            return true;
        };

        let attachments =
            extract_attachments(message, &install.alias, self.ingress.limits().attachments);
        let cleaned_text = raw_text.map(|t| {
            if is_group {
                install
                    .username
                    .as_deref()
                    .map_or_else(|| t.trim().to_owned(), |u| strip_bot_mention(t, u))
            } else {
                t.trim().to_owned()
            }
        });
        let base_text = match cleaned_text.filter(|t| !t.is_empty()) {
            Some(text) => text,
            None => match attachments.summary {
                Some(summary) => summary,
                None => return true,
            },
        };
        let text = match reply_context_prefix(message, install.bot_id) {
            Some(prefix) => format!("{prefix}\n{base_text}"),
            None => base_text,
        };

        let thread_id = is_group.then(|| forum_topic_id(message)).flatten();
        let request = format!("tg.{chat_id}.{message_id}");
        if !valid_request_id(&request) {
            return true;
        }
        let inbound = Inbound {
            real,
            principal,
            request: RequestId(request),
            conversation: Conversation {
                kind: if is_group {
                    ConversationKind::Channel
                } else {
                    ConversationKind::Direct
                },
                id: format!("{}/{chat_id}", install.bot_id),
                thread: thread_id.map(|id| id.to_string()),
            },
            attachments: attachments.refs,
            body: json!({"text": text}),
            event: Event::Message,
        };
        let admission = self.ingress.admit_with(&routes, inbound).await;
        if let Admission::Accepted { run, .. } = &admission {
            if is_group {
                self.reply_targets.record(run.clone(), message_id);
            }
            if let Some(ux) = &self.ux {
                ux.track(
                    TrackedRun {
                        profile: install.profile.clone(),
                        alias: install.alias.clone(),
                        chat_id,
                        message_id,
                        thread_id,
                        run: run.clone(),
                    },
                    stop.clone(),
                );
            }
        }
        admission.settled()
    }

    async fn on_callback(&self, install: &Installation, callback: &Value) -> bool {
        let Some(callback_id) = callback["id"].as_str().filter(|s| !s.is_empty()) else {
            return true;
        };
        let Some(from) = callback.get("from") else {
            self.answer_callback(install, callback_id, None).await;
            return true;
        };
        if from["is_bot"].as_bool() == Some(true) {
            self.answer_callback(install, callback_id, None).await;
            return true;
        }
        let Some(user_id) = positive_u64(&from["id"]) else {
            self.answer_callback(install, callback_id, None).await;
            return true;
        };
        let Some(data) = callback["data"].as_str() else {
            self.answer_callback(install, callback_id, None).await;
            return true;
        };
        let Some((allow, nonce)) = parse_callback(data) else {
            self.answer_callback(install, callback_id, None).await;
            return true;
        };
        let Some(message) = callback.get("message") else {
            self.answer_callback(install, callback_id, Some("Card message is unavailable."))
                .await;
            return true;
        };
        if message["chat"]["type"].as_str() != Some("private") {
            self.answer_callback(install, callback_id, None).await;
            return true;
        }
        let (Some(chat_id), Some(message_id)) = (
            positive_u64(&message["chat"]["id"]),
            positive_u64(&message["message_id"]),
        ) else {
            self.answer_callback(install, callback_id, None).await;
            return true;
        };
        if chat_id != user_id {
            self.answer_callback(install, callback_id, None).await;
            return true;
        }
        let started = now_nano();
        let Some(principal) = identity::telegram(user_id) else {
            self.answer_callback(install, callback_id, None).await;
            return true;
        };
        if self.ingress.verify_active().await.is_err() {
            self.answer_callback(install, callback_id, Some(RETRY_NOTICE))
                .await;
            observe::record_approval(
                &**self.ingress.tracer(),
                &install.profile,
                &principal.0,
                "unavailable",
                started,
            );
            return false;
        }
        let Ok(routes) = self.ingress.routes() else {
            self.answer_callback(install, callback_id, Some(RETRY_NOTICE))
                .await;
            return false;
        };
        let approval = ApprovalId(nonce.to_owned());
        let stored = match self.ledger.approval(approval.clone()).await {
            Ok(stored) => Some(stored),
            Err(Error::Unavailable | Error::Stale) => {
                self.answer_callback(install, callback_id, Some(RETRY_NOTICE))
                    .await;
                return false;
            }
            Err(_) => None,
        };
        // Group-origin owner-DM cards carry a delivery receipt and work even without a DM input binding.
        let snapshot = match stored.as_ref().and_then(|s| s.delivery.as_ref()) {
            Some(receipt) => {
                let Some(snapshot) = routes.snapshot(&install.profile).cloned() else {
                    self.answer_callback(install, callback_id, None).await;
                    return true;
                };
                if !is_the_card(receipt, install, &snapshot, chat_id, message_id, user_id) {
                    self.answer_callback(
                        install,
                        callback_id,
                        Some("Cannot approve using this card."),
                    )
                    .await;
                    observe::record_approval(
                        &**self.ingress.tracer(),
                        &install.profile,
                        &principal.0,
                        "wrong_card",
                        started,
                    );
                    return true;
                }
                snapshot
            }
            None => {
                let real = Real::TelegramDm {
                    bot_id: install.bot_id,
                };
                match routes
                    .find(&real)
                    .filter(|route| route.snapshot.id == install.profile)
                {
                    Some(route) => route.snapshot.clone(),
                    None => {
                        let state = stored
                            .as_ref()
                            .filter(|s| s.approval.attempt.profile == install.profile)
                            .map(|s| s.state);
                        let member = routes
                            .snapshot(&install.profile)
                            .is_some_and(|s| s.members.contains_key(&principal));
                        drop(routes);
                        if let (Some(state), true) = (state, member) {
                            let (outcome, toast) = if state == ApprovalState::Pending {
                                (
                                    "not_open",
                                    "This card is not open yet — please try again shortly.",
                                )
                            } else {
                                ("closed", "This request is already closed.")
                            };
                            self.answer_callback(install, callback_id, Some(toast))
                                .await;
                            observe::record_approval(
                                &**self.ingress.tracer(),
                                &install.profile,
                                &principal.0,
                                outcome,
                                started,
                            );
                        } else {
                            self.answer_callback(install, callback_id, None).await;
                        }
                        return true;
                    }
                }
            }
        };
        drop(routes);
        if !snapshot.members.contains_key(&principal) {
            self.answer_callback(
                install,
                callback_id,
                Some("Only registered users can act on this approval."),
            )
            .await;
            observe::record_approval(
                &**self.ingress.tracer(),
                &install.profile,
                &principal.0,
                "unregistered",
                started,
            );
            return true;
        }
        if !stored
            .as_ref()
            .is_some_and(|s| s.approval.attempt.profile == install.profile)
        {
            self.answer_callback(install, callback_id, Some("Unknown approval card."))
                .await;
            observe::record_approval(
                &**self.ingress.tracer(),
                &install.profile,
                &principal.0,
                "unknown_card",
                started,
            );
            return true;
        }
        let outcome = self
            .decide_callback(
                install,
                callback_id,
                &principal,
                allow,
                nonce,
                chat_id,
                message_id,
            )
            .await;
        observe::record_approval(
            &**self.ingress.tracer(),
            &install.profile,
            &principal.0,
            outcome,
            started,
        );
        outcome != "unavailable"
    }

    #[allow(clippy::too_many_arguments)]
    async fn decide_callback(
        &self,
        install: &Installation,
        callback_id: &str,
        principal: &bot_core::id::PrincipalId,
        allow: bool,
        nonce: &str,
        chat_id: u64,
        message_id: u64,
    ) -> &'static str {
        let approval = ApprovalId(nonce.to_owned());
        let (outcome, toast, refresh) =
            match self.broker.decide(approval, principal.clone(), allow).await {
                Ok(()) if allow => ("approved", "Approved", true),
                Ok(()) => ("denied", "Denied", true),
                Err(Error::Denied) => (
                    "forbidden",
                    "Only the designated approver can act on this approval.",
                    false,
                ),
                Err(Error::Stale | Error::Missing) => {
                    ("stale", "This card is no longer valid.", true)
                }
                Err(Error::Conflict) => {
                    match self.ledger.approval(ApprovalId(nonce.to_owned())).await {
                        Ok(latest) if status_line(latest.state).is_some() => (
                            "conflict",
                            "A different decision has already been recorded for this card.",
                            true,
                        ),
                        _ => ("unavailable", RETRY_NOTICE, false),
                    }
                }
                Err(_) => ("unavailable", RETRY_NOTICE, false),
            };
        self.answer_callback(install, callback_id, Some(toast))
            .await;
        if refresh {
            self.refresh_card(&install.profile, &install.alias, nonce, chat_id, message_id)
                .await;
        }
        outcome
    }

    async fn refresh_card(
        &self,
        profile: &ProfileId,
        alias: &str,
        nonce: &str,
        chat_id: u64,
        message_id: u64,
    ) {
        let Ok(stored) = self.ledger.approval(ApprovalId(nonce.to_owned())).await else {
            return;
        };
        let Some(status) = status_line(stored.state) else {
            return;
        };
        let BotCheck::Verified(token, _) = self.creds.check_bot(profile, alias).await else {
            return;
        };
        let text = serde_json::from_str::<Value>(&stored.card.0)
            .ok()
            .and_then(|v| parse_card(&v).ok())
            .map_or_else(|| status.to_owned(), |c| resolved_card_text(&c, status));
        let body = json!({
            "chat_id": chat_id,
            "message_id": message_id,
            "text": text,
            "reply_markup": {"inline_keyboard": []},
        });
        let first = self.creds.call_json("editMessageText", &token, &body).await;
        let retry_pause = match first {
            Raw::Ok(_) => return,
            Raw::RateLimited(wait) => Some(wait.min(Duration::from_secs(2))),
            Raw::BeforeWrite | Raw::TokenRejected | Raw::Unknown => {
                Some(Duration::from_millis(250))
            }
            Raw::Conflict | Raw::Error(_, _) => None,
        };
        if let Some(pause) = retry_pause {
            tokio::time::sleep(pause).await;
            if let BotCheck::Verified(fresh, _) = self.creds.check_bot(profile, alias).await
                && matches!(
                    self.creds.call_json("editMessageText", &fresh, &body).await,
                    Raw::Ok(_)
                )
            {
                return;
            }
        }
        tracing::warn!(
            profile = %profile.0,
            "failed to refresh Telegram approval card (decision already recorded)"
        );
    }

    async fn answer_callback(&self, install: &Installation, callback_id: &str, text: Option<&str>) {
        let Some(token) = self.creds.bot_token(&install.profile, &install.alias).await else {
            return;
        };
        let mut body = json!({"callback_query_id": callback_id});
        if let Some(text) = text {
            body["text"] = json!(text);
        }
        let _ = self
            .creds
            .call_json("answerCallbackQuery", &token, &body)
            .await;
    }
}
