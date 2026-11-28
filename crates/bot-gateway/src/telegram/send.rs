//! Telegram outbox reply sender for plain-text chunks, media uploads, and DM-only approval cards.
use super::{
    api::{MediaFile, MediaOptions, Raw},
    creds::{BotCheck, Credentials},
    render::{
        MAX_CAPTION_UTF16, MAX_UTF16_UNITS, card_fits, card_keyboard, card_text, plain_text,
        split_plain, utf16_len,
    },
    ux::ReplyTargets,
};
use crate::{
    delivery::{Delivery, Sender},
    reply::{Attachment, Card, ReplyBody},
};
use bot_core::{
    id::ProfileId,
    intake::{DeliveryScope, NotSentReason, Outgoing, PushTransportId},
    profile::{Adapter, Snapshot},
    state::ConversationKind,
};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

/// Telegram `sendPhoto` upload cap (10 MB); larger images fall back to `sendDocument`.
const MAX_PHOTO_BYTES: usize = 10 * 1024 * 1024;

pub(crate) enum CardPost {
    Posted {
        bot_id: String,
        dm: String,
        msg_id: String,
    },
    NotSent {
        reason: NotSentReason,
        retry_after: Option<Duration>,
    },
    Unavailable,
    Held,
}

#[derive(Clone, Copy)]
struct Target {
    chat_id: i64,
    thread_id: Option<u64>,
    reply_to_message_id: Option<u64>,
}

pub struct TelegramSender {
    creds: Arc<Credentials>,
    reply_targets: Arc<ReplyTargets>,
}

impl TelegramSender {
    pub(crate) fn new(creds: Arc<Credentials>, reply_targets: Arc<ReplyTargets>) -> Self {
        Self {
            creds,
            reply_targets,
        }
    }

    fn bot_alias<'a>(&self, snapshot: &'a Snapshot, item: &Outgoing) -> Option<&'a str> {
        match &snapshot.bindings.get(&item.binding)?.source {
            Adapter::TelegramDm { bot } | Adapter::TelegramGroups { bot } => Some(bot.as_str()),
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

    fn apply_routing(payload: &mut Value, target: Target, attach_reply: bool) {
        if let Some(thread_id) = target.thread_id {
            payload["message_thread_id"] = json!(thread_id);
        }
        if attach_reply && let Some(message_id) = target.reply_to_message_id {
            payload["reply_parameters"] = json!({
                "message_id": message_id,
                "allow_sending_without_reply": true,
            });
        }
    }

    async fn post_text_chunks(&self, token: &str, target: Target, text: &str) -> Delivery {
        let cleaned = plain_text(text);
        let chunks = split_plain(&cleaned, MAX_UTF16_UNITS);
        if chunks.is_empty() {
            return Delivery::Rejected;
        }
        let mut last_receipt = None;
        for (idx, chunk) in chunks.iter().enumerate() {
            let mut body = json!({
                "chat_id": target.chat_id,
                "text": chunk,
                "link_preview_options": {"is_disabled": true},
            });
            Self::apply_routing(&mut body, target, idx == 0);
            let raw = self.creds.call_json("sendMessage", token, &body).await;
            match classify_delivery(raw, target.chat_id, idx > 0) {
                Step::Sent(receipt) => last_receipt = Some(receipt),
                Step::Done(delivery) => return delivery,
            }
        }
        match last_receipt {
            Some(receipt) => Delivery::Sent { receipt },
            None => Delivery::Rejected,
        }
    }

    async fn post_approval(&self, token: &str, chat_id: i64, body: &ReplyBody) -> Delivery {
        let Some(card) = &body.approval else {
            return Delivery::Rejected;
        };
        let (text, markup) = if card_fits(card) {
            let mut text = card_text(card);
            if let Some(prefix) = &body.text {
                let prefix = plain_text(prefix);
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

    /// Posts a group-origin approval card directly into the approver's 1:1 Telegram DM.
    pub(crate) async fn post_owner_card(
        &self,
        profile: &ProfileId,
        bot_alias: &str,
        user_id: u64,
        card: &Card,
    ) -> CardPost {
        let Ok(chat_id) = i64::try_from(user_id) else {
            return CardPost::Unavailable;
        };
        let (token, bot_id) = match self.creds.check_bot(profile, bot_alias).await {
            BotCheck::Verified(token, info) => (token, info.id),
            BotCheck::Unreadable | BotCheck::Unverified => {
                return CardPost::NotSent {
                    reason: NotSentReason::BeforeWrite,
                    retry_after: None,
                };
            }
            BotCheck::WrongBot => return CardPost::Unavailable,
        };
        let (text, markup) = if card_fits(card) {
            (card_text(card), card_keyboard(card))
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
        let raw = self.creds.call_json("sendMessage", &token, &payload).await;
        match raw {
            Raw::Ok(value) => match confirms_message_id(chat_id, &value) {
                Some(msg_id) => CardPost::Posted {
                    bot_id: bot_id.to_string(),
                    dm: user_id.to_string(),
                    msg_id: msg_id.to_string(),
                },
                None => CardPost::Held,
            },
            Raw::BeforeWrite | Raw::TokenRejected => CardPost::NotSent {
                reason: NotSentReason::BeforeWrite,
                retry_after: None,
            },
            Raw::RateLimited(wait) => CardPost::NotSent {
                reason: NotSentReason::RateLimited,
                retry_after: Some(wait),
            },
            Raw::Error(_, _) => CardPost::Unavailable,
            Raw::Conflict | Raw::Unknown => CardPost::Held,
        }
    }

    async fn upload_attachment(
        &self,
        token: &str,
        chat_str: &str,
        attachment: &Attachment,
        options: MediaOptions<'_>,
    ) -> Raw {
        let file = MediaFile {
            name: &attachment.name,
            mime: &attachment.mime,
            bytes: &attachment.data,
        };
        if is_telegram_photo(attachment) {
            let raw = self
                .creds
                .api()
                .send_photo(token, chat_str, file, options)
                .await;
            // Confirmed 400 rejection (e.g. aspect ratio / resolution) means nothing was sent; fall back to sendDocument.
            if !matches!(raw, Raw::Error(400, _)) {
                return raw;
            }
        }
        self.creds
            .api()
            .send_document(token, chat_str, file, options)
            .await
    }

    async fn post_attachments(&self, token: &str, target: Target, body: &ReplyBody) -> Delivery {
        let mut sent_any = false;
        let cleaned_text = body.text.as_deref().map(plain_text);
        let single_caption = if body.attachments.len() == 1 {
            cleaned_text
                .as_deref()
                .filter(|t| utf16_len(t) <= MAX_CAPTION_UTF16)
        } else {
            None
        };
        if single_caption.is_none()
            && let Some(text) = cleaned_text.as_deref()
        {
            match self.post_text_chunks(token, target, text).await {
                Delivery::Sent { .. } => sent_any = true,
                other => return other,
            }
        }
        let chat_str = target.chat_id.to_string();
        let mut last_receipt = None;
        for (idx, attachment) in body.attachments.iter().enumerate() {
            let options = MediaOptions {
                caption: (idx == 0).then_some(single_caption).flatten(),
                thread_id: target.thread_id,
                reply_to_message_id: (!sent_any && idx == 0)
                    .then_some(target.reply_to_message_id)
                    .flatten(),
            };
            let raw = self
                .upload_attachment(token, &chat_str, attachment, options)
                .await;
            if matches!(raw, Raw::TokenRejected) {
                self.creds.reject(token);
            }
            match classify_delivery(raw, target.chat_id, sent_any) {
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

fn is_telegram_photo(attachment: &Attachment) -> bool {
    attachment.data.len() <= MAX_PHOTO_BYTES
        && matches!(
            attachment.mime.as_str(),
            "image/jpeg" | "image/png" | "image/webp"
        )
}

enum Step {
    Sent(String),
    Done(Delivery),
}

fn confirms_message_id(chat_id: i64, value: &Value) -> Option<u64> {
    let returned_chat = value["chat"]["id"].as_i64().or_else(|| {
        value["chat"]["id"]
            .as_u64()
            .and_then(|n| i64::try_from(n).ok())
    })?;
    let message_id = value["message_id"].as_u64().filter(|id| *id > 0)?;
    (returned_chat == chat_id).then_some(message_id)
}

fn confirms_message(chat_id: i64, value: &Value) -> Option<String> {
    confirms_message_id(chat_id, value).map(|message_id| format!("{chat_id}:{message_id}"))
}

fn classify_delivery(raw: Raw, chat_id: i64, partial_sent: bool) -> Step {
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

/// Splits canonical `<bot_id>/<chat_id>` destination into positive `bot_id` and signed non-zero `chat_id`.
pub(crate) fn split_destination(id: &str) -> Option<(u64, i64)> {
    let (bot, chat) = id.split_once('/')?;
    let bot_id = bot.parse::<u64>().ok().filter(|n| *n > 0)?;
    let chat_id = chat.parse::<i64>().ok().filter(|n| *n != 0)?;
    Some((bot_id, chat_id))
}

fn parse_thread_id(thread: Option<&str>) -> Option<Option<u64>> {
    match thread {
        None => Some(None),
        Some(raw) => raw.parse::<u64>().ok().filter(|n| *n > 0).map(Some),
    }
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
        let Some((bot_id, chat_id)) = split_destination(&item.destination.id) else {
            return false;
        };
        match (&binding.source, &item.destination.kind) {
            (Adapter::TelegramDm { bot }, ConversationKind::Direct)
                if chat_id > 0 && item.destination.thread.is_none() =>
            {
                self.creds
                    .host()
                    .telegram
                    .bots
                    .get(bot)
                    .is_some_and(|entry| entry.dm && entry.bot_id == bot_id)
            }
            (Adapter::TelegramGroups { bot }, ConversationKind::Channel)
                if chat_id < 0 && parse_thread_id(item.destination.thread.as_deref()).is_some() =>
            {
                self.creds
                    .host()
                    .telegram
                    .bots
                    .get(bot)
                    .is_some_and(|entry| entry.groups && entry.bot_id == bot_id)
            }
            _ => false,
        }
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
            if body.approval.is_some()
                && (!body.attachments.is_empty()
                    || item.destination.kind == ConversationKind::Channel)
            {
                // Never post approval cards or inline buttons into a shared group chat.
                return Delivery::Rejected;
            }
            let Some(bot_alias) = self.bot_alias(snapshot, item) else {
                return Delivery::Rejected;
            };
            let Some((_, chat_id)) = split_destination(&item.destination.id) else {
                return Delivery::Rejected;
            };
            let Some(thread_id) = parse_thread_id(item.destination.thread.as_deref()) else {
                return Delivery::Rejected;
            };
            let reply_to_message_id = (item.destination.kind == ConversationKind::Channel)
                .then(|| self.reply_targets.get(&item.key.run))
                .flatten();
            let target = Target {
                chat_id,
                thread_id,
                reply_to_message_id,
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
                    Some(text) => self.post_text_chunks(&token, target, text).await,
                    None => Delivery::Rejected,
                },
                (false, None) => self.post_attachments(&token, target, body).await,
                (false, Some(_)) => Delivery::Rejected,
            }
        })
    }
}
