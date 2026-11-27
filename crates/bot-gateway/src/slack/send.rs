//! Slack reply sender for markdown messages, approval cards, and external file uploads.
use super::{
    api::{Body, Raw, SlackApi},
    blocks::{
        PLAIN_LIMIT, card_blocks, card_fits, card_text, fits_block, markdown_block, neutralize,
    },
    creds::{BotCheck, Credentials},
};
use crate::{
    delivery::{Delivery, Sender},
    reply::ReplyBody,
    schedule_routes::{is_ts, split_route},
};
use bot_core::{
    intake::{DeliveryScope, NotSentReason, Outgoing, PushTransportId},
    profile::{Adapter, Snapshot},
    slack::{BotThread, BotThreads},
    state::ConversationKind,
};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, sync::Arc};

/// Block-format rejections where Slack guarantees nothing was posted, making plain-text fallback safe.
const FORMAT_ERRORS: [&str; 3] = [
    "invalid_blocks",
    "invalid_blocks_format",
    "msg_blocks_too_long",
];
/// Server-side `ok: false` codes with ambiguous execution outcome (`Held`).
const UNKNOWN_ERRORS: [&str; 5] = [
    "internal_error",
    "fatal_error",
    "service_unavailable",
    "request_timeout",
    "timeout",
];

pub struct SlackSender {
    api: SlackApi,
    creds: Arc<Credentials>,
    threads: Option<Arc<dyn BotThreads>>,
    ux: Option<Arc<super::ux::SlackUx>>,
}

impl SlackSender {
    pub fn new(api: SlackApi, creds: Arc<Credentials>) -> Self {
        Self {
            api,
            creds,
            threads: None,
            ux: None,
        }
    }

    #[must_use]
    pub fn with_threads(mut self, threads: Arc<dyn BotThreads>) -> Self {
        self.threads = Some(threads);
        self
    }

    #[must_use]
    pub fn with_ux(mut self, ux: Arc<super::ux::SlackUx>) -> Self {
        self.ux = Some(ux);
        self
    }

    async fn note_thread(
        &self,
        snapshot: &Snapshot,
        item: &Outgoing,
        (workspace, bot): (&str, &str),
        (channel, thread): (&str, Option<&str>),
        receipt: &str,
    ) {
        let Some(threads) = &self.threads else {
            return;
        };
        let root = match thread {
            Some(thread) => thread.to_owned(),
            // Message receipts are `channel:ts`; file upload receipts are `channel:file_id` and lack a post ts.
            None => match receipt.split_once(':') {
                Some((_, ts)) if is_ts(ts) => ts.to_owned(),
                _ => return,
            },
        };
        let Some(team) = self
            .creds
            .host()
            .slack
            .workspaces
            .get(workspace)
            .map(|ws| ws.team_id.clone())
        else {
            return;
        };
        let noted = threads
            .note(BotThread {
                profile: snapshot.id.clone(),
                binding: item.binding.clone(),
                team,
                bot: bot.to_owned(),
                channel: channel.to_owned(),
                root_ts: root,
            })
            .await;
        if noted.is_err() {
            tracing::warn!(
                "failed to record bot thread participation — human replies will be ignored until confirmed by own-post event"
            );
        }
    }
}

enum Step {
    Done(Delivery),
    Format,
}

/// Treats Slack's `message_truncated` warning as `Held` because partial content was already posted.
fn truncated(value: &Value) -> bool {
    value["response_metadata"]["warnings"]
        .as_array()
        .is_some_and(|warnings| warnings.iter().any(|w| w == "message_truncated"))
}

fn step(raw: Raw, done: impl FnOnce(&Value) -> Option<String>) -> Step {
    match raw {
        Raw::Ok(value) if truncated(&value) => Step::Done(Delivery::Held),
        Raw::Ok(value) => match done(&value) {
            Some(receipt) => Step::Done(Delivery::Sent { receipt }),
            None => Step::Done(Delivery::Held),
        },
        Raw::Error(code) if FORMAT_ERRORS.contains(&code.as_str()) => Step::Format,
        Raw::Error(code) if UNKNOWN_ERRORS.contains(&code.as_str()) => Step::Done(Delivery::Held),
        Raw::Error(_) => Step::Done(Delivery::Rejected),
        Raw::BeforeWrite | Raw::TokenRejected => Step::Done(Delivery::NotSent {
            reason: NotSentReason::BeforeWrite,
            retry_after: None,
        }),
        Raw::RateLimited(wait) => Step::Done(Delivery::NotSent {
            reason: NotSentReason::RateLimited,
            retry_after: Some(wait),
        }),
        Raw::Unknown => Step::Done(Delivery::Held),
    }
}

fn sendable(body: &ReplyBody) -> bool {
    if body.approval.is_some() && !body.attachments.is_empty() {
        return false;
    }
    match body.text.as_deref() {
        Some(text) if !fits_block(text) => {
            body.approval.is_none() && neutralize(text).chars().count() <= PLAIN_LIMIT
        }
        _ => true,
    }
}

/// Confirms a post only when Slack echoes the target channel, a valid `ts`, and no truncation warning.
#[doc(hidden)]
pub fn confirms_post(channel: &str, reply: &Value) -> Option<(String, String)> {
    if truncated(reply) {
        return None;
    }
    let ts = reply["ts"].as_str().filter(|ts| is_ts(ts))?;
    (reply["channel"].as_str()? == channel).then(|| (channel.to_owned(), ts.to_owned()))
}

fn receipt_of(channel: &str, value: &Value) -> Option<String> {
    confirms_post(channel, value).map(|(channel, ts)| format!("{channel}:{ts}"))
}

fn upload_slot(value: &Value) -> Option<(String, String)> {
    let url = value["upload_url"].as_str().filter(|url| !url.is_empty())?;
    let id = value["file_id"].as_str().filter(|id| !id.is_empty())?;
    Some((url.to_owned(), id.to_owned()))
}

/// Confirms upload completion only if the returned file IDs exactly match the unique requested IDs.
fn files_receipt(channel: &str, value: &Value, requested: &[&str]) -> Option<String> {
    if !unique_ids(requested) {
        return None;
    }
    let mut returned = value["files"]
        .as_array()?
        .iter()
        .map(|file| file["id"].as_str())
        .collect::<Option<Vec<&str>>>()?;
    returned.sort_unstable();
    let mut wanted = requested.to_vec();
    wanted.sort_unstable();
    (returned == wanted).then(|| format!("{channel}:{}", requested.join(",")))
}

fn unique_ids(ids: &[&str]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    ids.iter().all(|id| !id.is_empty() && seen.insert(*id))
}

impl SlackSender {
    fn workspace<'a>(&self, snapshot: &'a Snapshot, item: &Outgoing) -> Option<&'a str> {
        match &snapshot.bindings.get(&item.binding)?.source {
            Adapter::SlackChannel { workspace, .. } | Adapter::SlackDm { workspace } => {
                Some(workspace)
            }
            _ => None,
        }
    }

    async fn post(
        &self,
        token: &str,
        channel: &str,
        thread: Option<&str>,
        body: &ReplyBody,
    ) -> Delivery {
        let card = body.approval.as_ref();
        // Never truncate message text: use plain text when it exceeds the markdown block limit.
        if body.text.as_deref().is_some_and(|text| !fits_block(text)) {
            return match (card, body.text.as_deref()) {
                (None, Some(text)) => {
                    self.post_plain(token, channel, thread, &neutralize(text))
                        .await
                }
                _ => Delivery::Rejected,
            };
        }
        let mut blocks: Vec<Value> = body.text.iter().map(|t| markdown_block(t)).collect();
        match card {
            Some(card) if card_fits(card) => blocks.extend(card_blocks(card)),
            Some(_) => {
                // Cards that cannot fit intact never render buttons; direct the user to the admin UI.
                let notice = self.notice();
                blocks.push(
                    json!({"type": "section", "text": {"type": "plain_text", "text": notice}}),
                );
            }
            None => {}
        }
        let mut message = json!({"channel": channel, "blocks": blocks, "text": self.summary(body),
            "unfurl_links": false, "unfurl_media": false});
        if let Some(thread) = thread {
            message["thread_ts"] = json!(thread);
        }
        let raw = self
            .creds
            .call("chat.postMessage", token, Body::Json(&message))
            .await;
        match step(raw, |v| receipt_of(channel, v)) {
            Step::Done(delivery) => delivery,
            // Approval cards cannot fall back to plain text because they require interactive buttons.
            Step::Format if card.is_some() => Delivery::Rejected,
            Step::Format => match body.text.as_deref() {
                Some(text) => {
                    self.post_plain(token, channel, thread, &neutralize(text))
                        .await
                }
                None => Delivery::Rejected,
            },
        }
    }

    async fn post_plain(
        &self,
        token: &str,
        channel: &str,
        thread: Option<&str>,
        text: &str,
    ) -> Delivery {
        if text.chars().count() > PLAIN_LIMIT {
            return Delivery::Rejected;
        }
        let mut plain = json!({"channel": channel, "text": text,
            "unfurl_links": false, "unfurl_media": false});
        if let Some(thread) = thread {
            plain["thread_ts"] = json!(thread);
        }
        let raw = self
            .creds
            .call("chat.postMessage", token, Body::Json(&plain))
            .await;
        match step(raw, |v| receipt_of(channel, v)) {
            Step::Done(delivery) => delivery,
            Step::Format => Delivery::Rejected,
        }
    }

    /// Top-level fallback `text` for notifications/screen readers with broadcast mentions neutralized.
    fn summary(&self, body: &ReplyBody) -> String {
        let mut parts: Vec<String> = body.text.iter().map(|text| neutralize(text)).collect();
        match &body.approval {
            Some(card) if card_fits(card) => parts.push(neutralize(&card_text(card))),
            Some(_) => parts.push(self.notice()),
            None => {}
        }
        parts.join("\n\n")
    }

    fn notice(&self) -> String {
        match &self.creds.host().admin_url {
            Some(url) => format!(
                "Approval required. Content is too long to display here — review and approve in the admin UI: {url}"
            ),
            None => "Approval required. Content is too long to display here — review and approve in the admin UI.".to_owned(),
        }
    }

    /// Three-step external upload; only `files.completeUploadExternal` publishes to the channel.
    async fn upload(
        &self,
        token: &str,
        channel: &str,
        thread: Option<&str>,
        body: &ReplyBody,
    ) -> Delivery {
        let mut files: Vec<Value> = Vec::new();
        for attachment in &body.attachments {
            let form = vec![
                ("filename", attachment.name.clone()),
                ("length", attachment.data.len().to_string()),
            ];
            let slot = self
                .creds
                .call("files.getUploadURLExternal", token, Body::Form(form))
                .await;
            let (url, id) = match slot {
                Raw::Ok(value) => match upload_slot(&value) {
                    Some(slot) => slot,
                    None => return Delivery::Rejected,
                },
                raw => match step(raw, |_| None) {
                    Step::Done(Delivery::Sent { .. }) | Step::Format => return Delivery::Rejected,
                    Step::Done(other) => return other,
                },
            };
            if files.iter().any(|f| f["id"] == id.as_str()) {
                return Delivery::Rejected;
            }
            match self.api.put_bytes(&url, attachment.data.clone()).await {
                Raw::Ok(_) => {}
                raw => match step(raw, |_| None) {
                    Step::Done(Delivery::Sent { .. }) | Step::Format => {
                        return Delivery::Rejected;
                    }
                    Step::Done(other) => return other,
                },
            }
            files.push(json!({"id": id, "title": attachment.name}));
        }
        let mut form = vec![
            ("files", Value::Array(files.clone()).to_string()),
            ("channel_id", channel.to_owned()),
        ];
        if let Some(thread) = thread {
            form.push(("thread_ts", thread.to_owned()));
        }
        if let Some(text) = &body.text {
            form.push(("initial_comment", neutralize(text)));
        }
        let ids: Vec<&str> = files.iter().filter_map(|f| f["id"].as_str()).collect();
        let raw = self
            .creds
            .call("files.completeUploadExternal", token, Body::Form(form))
            .await;
        match step(raw, |v| files_receipt(channel, v, &ids)) {
            Step::Done(delivery) => delivery,
            Step::Format => Delivery::Rejected,
        }
    }
}

impl Sender for SlackSender {
    fn scope(&self) -> DeliveryScope {
        DeliveryScope::all(PushTransportId::SLACK)
    }

    fn allows(&self, snapshot: &Snapshot, item: &Outgoing) -> bool {
        let Some(binding) = snapshot
            .bindings
            .get(&item.binding)
            .filter(|_| item.transport == PushTransportId::SLACK)
        else {
            return false;
        };
        let destination = &item.destination;
        let place = match (&binding.source, &destination.kind) {
            (
                Adapter::SlackChannel {
                    workspace, channel, ..
                },
                ConversationKind::Channel,
            ) => {
                let host = self.creds.host();
                host.slack
                    .workspaces
                    .get(workspace)
                    .and_then(|ws| Some((ws, ws.channels.get(channel)?)))
                    .is_some_and(|(ws, id)| match split_route(&destination.id) {
                        Some((team, chosen)) => team == ws.team_id && chosen == id,
                        None => *id == destination.id,
                    })
            }
            (Adapter::SlackDm { .. }, ConversationKind::Direct) => destination.id.starts_with('D'),
            _ => false,
        };
        place && destination.thread.as_deref().is_none_or(is_ts)
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
            let Some(workspace) = self.workspace(snapshot, item) else {
                return Delivery::Rejected;
            };
            if !sendable(body) {
                return Delivery::Rejected;
            }
            let (token, bot) = match self.creds.check_bot(&item.key.profile, workspace).await {
                BotCheck::Verified(token, auth) => (token, auth.user_id),
                _ => {
                    return Delivery::NotSent {
                        reason: NotSentReason::BeforeWrite,
                        retry_after: None,
                    };
                }
            };
            // Strip canonical `TEAM/CHANNEL` prefix after `allows` verified the team.
            let channel = split_route(&item.destination.id)
                .map_or(item.destination.id.as_str(), |(_, channel)| channel);
            let thread = item.destination.thread.as_deref();
            if let (Some(ux), Some(thread)) = (&self.ux, thread) {
                ux.clear_stream(
                    &item.key.profile,
                    workspace,
                    channel,
                    thread,
                    &item.key.run,
                    body.approval.is_none(),
                )
                .await;
            }
            let delivery = match (body.attachments.is_empty(), &body.approval) {
                (true, _) => self.post(&token, channel, thread, body).await,
                (false, None) => self.upload(&token, channel, thread, body).await,
                (false, Some(_)) => Delivery::Rejected,
            };
            if let (Delivery::Sent { receipt }, ConversationKind::Channel) =
                (&delivery, &item.destination.kind)
            {
                self.note_thread(
                    snapshot,
                    item,
                    (workspace, &bot),
                    (channel, thread),
                    receipt,
                )
                .await;
            }
            delivery
        })
    }
}

mod card;
pub use card::{CardPost, slack_user};

#[cfg(test)]
mod tests;
