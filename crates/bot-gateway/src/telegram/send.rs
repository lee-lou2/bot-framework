//! Telegram outbox reply sender for plain-text chunks, document attachments, and inline approval cards.
use super::{
    api::Raw,
    creds::{BotCheck, Credentials},
    render::{
        MAX_CAPTION_UTF16, MAX_UTF16_UNITS, card_fits, card_keyboard, card_text, split_plain,
        utf16_len,
    },
};
use crate::{
    delivery::{Delivery, Sender},
    reply::ReplyBody,
};
use bot_core::{
    intake::{DeliveryScope, NotSentReason, Outgoing, PushTransportId},
    profile::{Adapter, Snapshot},
    state::ConversationKind,
};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, sync::Arc};

pub struct TelegramSender {
    creds: Arc<Credentials>,
}

impl TelegramSender {
    pub fn new(creds: Arc<Credentials>) -> Self {
        Self { creds }
    }

    fn bot_alias<'a>(&self, snapshot: &'a Snapshot, item: &Outgoing) -> Option<&'a str> {
        match &snapshot.bindings.get(&item.binding)?.source {
            Adapter::TelegramDm { bot } => Some(bot.as_str()),
            _ => None,
        }
    }

    fn notice(&self) -> String {
        match &self.creds.host().admin_url {
            Some(url) => format!(
                "Approval required. Content is too long to display here — review and approve in the admin UI: {url}"
            ),
            None => "Approval required. Content is too long to display here — review and approve in the admin UI.".to_owned(),
        }
    }

    async fn post_text_chunks(&self, token: &str, chat_id: u64, text: &str) -> Delivery {
        let chunks = split_plain(text, MAX_UTF16_UNITS);
        if chunks.is_empty() {
            return Delivery::Rejected;
        }
        let mut last_receipt = None;
        for (idx, chunk) in chunks.iter().enumerate() {
            let body = json!({
                "chat_id": chat_id,
                "text": chunk,
                "link_preview_options": {"is_disabled": true},
            });
            let raw = self.creds.call_json("sendMessage", token, &body).await;
            match classify_delivery(raw, chat_id, idx > 0) {
                Step::Sent(receipt) => last_receipt = Some(receipt),
                Step::Done(delivery) => return delivery,
            }
        }
        match last_receipt {
            Some(receipt) => Delivery::Sent { receipt },
            None => Delivery::Rejected,
        }
    }

    async fn post_approval(&self, token: &str, chat_id: u64, body: &ReplyBody) -> Delivery {
        let Some(card) = &body.approval else {
            return Delivery::Rejected;
        };
        let (text, markup) = if card_fits(card) {
            let mut text = card_text(card);
            if let Some(prefix) = &body.text {
                let combined = format!("{prefix}\n\n{text}");
                if utf16_len(&combined) <= MAX_UTF16_UNITS {
                    text = combined;
                }
            }
            (text, card_keyboard(card))
        } else {
            (self.notice(), None)
        };
        let mut payload = json!({
            "chat_id": chat_id,
            "text": text,
            "link_preview_options": {"is_disabled": true},
        });
        if let Some(markup) = markup {
            payload["reply_markup"] = markup;
        }
        let raw = self.creds.call_json("sendMessage", token, &payload).await;
        match classify_delivery(raw, chat_id, false) {
            Step::Sent(receipt) => Delivery::Sent { receipt },
            Step::Done(delivery) => delivery,
        }
    }

    async fn post_attachments(&self, token: &str, chat_id: u64, body: &ReplyBody) -> Delivery {
        let mut sent_any = false;
        let single_caption = if body.attachments.len() == 1 {
            body.text
                .as_deref()
                .filter(|t| utf16_len(t) <= MAX_CAPTION_UTF16)
        } else {
            None
        };
        if single_caption.is_none()
            && let Some(text) = body.text.as_deref()
        {
            match self.post_text_chunks(token, chat_id, text).await {
                Delivery::Sent { .. } => sent_any = true,
                other => return other,
            }
        }
        let chat_str = chat_id.to_string();
        let mut last_receipt = None;
        for attachment in &body.attachments {
            let raw = self
                .creds
                .api()
                .send_document(
                    token,
                    &chat_str,
                    &attachment.name,
                    &attachment.mime,
                    &attachment.data,
                    single_caption,
                )
                .await;
            if matches!(raw, Raw::TokenRejected) {
                self.creds.reject(token);
            }
            match classify_delivery(raw, chat_id, sent_any) {
                Step::Sent(receipt) => {
                    sent_any = true;
                    last_receipt = Some(receipt);
                }
                Step::Done(delivery) => return delivery,
            }
        }
        match last_receipt {
            Some(receipt) => Delivery::Sent { receipt },
            None => Delivery::Rejected,
        }
    }
}

enum Step {
    Sent(String),
    Done(Delivery),
}

fn confirms_message(chat_id: u64, value: &Value) -> Option<String> {
    let returned_chat = value["chat"]["id"].as_u64().or_else(|| {
        value["chat"]["id"]
            .as_i64()
            .and_then(|n| u64::try_from(n).ok())
    })?;
    let message_id = value["message_id"].as_u64().filter(|id| *id > 0)?;
    (returned_chat == chat_id).then(|| format!("{chat_id}:{message_id}"))
}

fn classify_delivery(raw: Raw, chat_id: u64, partial_sent: bool) -> Step {
    match raw {
        Raw::Ok(value) => match confirms_message(chat_id, &value) {
            Some(receipt) => Step::Sent(receipt),
            None => Step::Done(Delivery::Held),
        },
        // Once any chunk/document in a multi-part reply has been delivered, settle later errors as Held to prevent duplicates.
        _ if partial_sent => Step::Done(Delivery::Held),
        Raw::BeforeWrite | Raw::TokenRejected => Step::Done(Delivery::NotSent {
            reason: NotSentReason::BeforeWrite,
            retry_after: None,
        }),
        Raw::RateLimited(wait) => Step::Done(Delivery::NotSent {
            reason: NotSentReason::RateLimited,
            retry_after: Some(wait),
        }),
        Raw::Error(_, _) => Step::Done(Delivery::Rejected),
        Raw::Conflict | Raw::Unknown => Step::Done(Delivery::Held),
    }
}

/// Splits canonical `<bot_id>/<chat_id>` DM destination into positive numeric IDs.
pub(crate) fn split_dm_destination(id: &str) -> Option<(u64, u64)> {
    let (bot, chat) = id.split_once('/')?;
    let bot_id = bot.parse::<u64>().ok().filter(|n| *n > 0)?;
    let chat_id = chat.parse::<u64>().ok().filter(|n| *n > 0)?;
    Some((bot_id, chat_id))
}

impl Sender for TelegramSender {
    fn scope(&self) -> DeliveryScope {
        DeliveryScope::all(PushTransportId::TELEGRAM)
    }

    fn allows(&self, snapshot: &Snapshot, item: &Outgoing) -> bool {
        let Some(binding) = snapshot
            .bindings
            .get(&item.binding)
            .filter(|_| item.transport == PushTransportId::TELEGRAM)
        else {
            return false;
        };
        let Adapter::TelegramDm { bot } = &binding.source else {
            return false;
        };
        if item.destination.kind != ConversationKind::Direct || item.destination.thread.is_some() {
            return false;
        }
        let Some((bot_id, _chat_id)) = split_dm_destination(&item.destination.id) else {
            return false;
        };
        self.creds
            .host()
            .telegram
            .bots
            .get(bot)
            .is_some_and(|entry| entry.dm && entry.bot_id == bot_id)
    }

    fn send<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        item: &'a Outgoing,
        body: &'a ReplyBody,
    ) -> Pin<Box<dyn Future<Output = Delivery> + Send + 'a>> {
        Box::pin(async move {
            if !self.allows(snapshot, item) {
                return Delivery::Rejected;
            }
            if body.approval.is_some() && !body.attachments.is_empty() {
                return Delivery::Rejected;
            }
            let Some(bot_alias) = self.bot_alias(snapshot, item) else {
                return Delivery::Rejected;
            };
            let Some((_, chat_id)) = split_dm_destination(&item.destination.id) else {
                return Delivery::Rejected;
            };
            let BotCheck::Verified(token, _) =
                self.creds.check_bot(&item.key.profile, bot_alias).await
            else {
                return Delivery::NotSent {
                    reason: NotSentReason::BeforeWrite,
                    retry_after: None,
                };
            };
            match (body.attachments.is_empty(), &body.approval) {
                (true, Some(_)) => self.post_approval(&token, chat_id, body).await,
                (true, None) => match body.text.as_deref() {
                    Some(text) => self.post_text_chunks(&token, chat_id, text).await,
                    None => Delivery::Rejected,
                },
                (false, None) => self.post_attachments(&token, chat_id, body).await,
                (false, Some(_)) => Delivery::Rejected,
            }
        })
    }
}
