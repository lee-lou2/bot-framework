//! Telegram update dispatch: private DM messages to `Ingress` and approval callbacks to `Broker::decide`.
//!
//! Never falls back across profiles: updates whose bot or route does not match this installation are ignored.
use super::{
    Installation, TelegramGateway,
    creds::BotCheck,
    render::{parse_callback, resolved_card_text, status_line},
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
    id::{ApprovalId, ProfileId, RequestId, RunId},
    port::Error,
    state::{Conversation, ConversationKind, RunState},
    trace::now_nano,
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

const TYPING_EVERY: Duration = Duration::from_secs(4);
const TYPING_MAX: Duration = Duration::from_secs(300);
const RETRY_NOTICE: &str = "Could not process right now — please try clicking again shortly.";

fn positive_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .filter(|id| *id > 0)
}

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
        if chat["type"].as_str() != Some("private") {
            return true;
        }
        let Some(from) = message.get("from") else {
            return true;
        };
        if from["is_bot"].as_bool() == Some(true) {
            return true;
        }
        let (Some(user_id), Some(chat_id), Some(message_id)) = (
            positive_u64(&from["id"]),
            positive_u64(&chat["id"]),
            positive_u64(&message["message_id"]),
        ) else {
            return true;
        };
        if user_id != chat_id {
            return true;
        }
        let text = message["text"]
            .as_str()
            .or_else(|| message["caption"].as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let Some(text) = text else {
            return true;
        };
        let real = Real::TelegramDm {
            bot_id: install.bot_id,
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
        let request = format!("tg.{chat_id}.{message_id}");
        if !valid_request_id(&request) {
            return true;
        }
        let inbound = Inbound {
            real,
            principal,
            request: RequestId(request),
            conversation: Conversation {
                kind: ConversationKind::Direct,
                id: format!("{}/{chat_id}", install.bot_id),
                thread: None,
            },
            attachments: Vec::new(),
            body: json!({"text": text}),
            event: Event::Message,
        };
        let admission = self.ingress.admit_with(&routes, inbound).await;
        if let Admission::Accepted { run, .. } = &admission {
            self.spawn_typing(install.clone(), chat_id, run.clone(), stop.clone());
        }
        admission.settled()
    }

    fn spawn_typing(
        &self,
        install: Installation,
        chat_id: u64,
        run: RunId,
        stop: CancellationToken,
    ) {
        let Some(runs) = self.runs.clone() else {
            return;
        };
        let creds = self.creds.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            loop {
                if let Some(token) = creds.bot_token(&install.profile, &install.alias).await {
                    let _ = creds
                        .api()
                        .call_json(
                            "sendChatAction",
                            &token,
                            &json!({"chat_id": chat_id, "action": "typing"}),
                        )
                        .await;
                }
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(TYPING_EVERY) => {}
                }
                if started.elapsed() >= TYPING_MAX {
                    return;
                }
                match runs.progress(install.profile.clone(), run.clone()).await {
                    Ok(Some(progress))
                        if matches!(progress.state, RunState::Ready | RunState::Leased)
                            && !progress.waiting_approval => {}
                    _ => return,
                }
            }
        });
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
        let Ok(routes) = self.ingress.routes() else {
            self.answer_callback(install, callback_id, Some(RETRY_NOTICE))
                .await;
            return true;
        };
        let real = Real::TelegramDm {
            bot_id: install.bot_id,
        };
        let Some(snapshot) = routes
            .find(&real)
            .filter(|route| route.snapshot.id == install.profile)
            .map(|route| route.snapshot.clone())
        else {
            self.answer_callback(install, callback_id, None).await;
            return true;
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
        true
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
        if self.ingress.verify_active().await.is_err() {
            self.answer_callback(install, callback_id, Some(RETRY_NOTICE))
                .await;
            return "unavailable";
        }
        let approval = ApprovalId(nonce.to_owned());
        let stored = match self.ledger.approval(approval.clone()).await {
            Ok(stored) => stored,
            Err(Error::Unavailable | Error::Stale) => {
                self.answer_callback(install, callback_id, Some(RETRY_NOTICE))
                    .await;
                return "unavailable";
            }
            Err(_) => {
                self.answer_callback(install, callback_id, Some("Unknown approval card."))
                    .await;
                return "unknown_card";
            }
        };
        if stored.approval.attempt.profile != install.profile {
            self.answer_callback(install, callback_id, Some("Unknown approval card."))
                .await;
            return "unknown_card";
        }
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
        let _ = self.creds.call_json("editMessageText", &token, &body).await;
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
