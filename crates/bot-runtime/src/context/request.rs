//! Per-adapter host-authenticated request context rendered into the system prompt.
//!
//! Only host-verified identifiers from `RunContext` and `Snapshot` enter this section, never
//! caller-supplied payload text. Every value is scrubbed to a single control-free line so
//! malformed identifiers or attachment refs cannot forge prompt headings or instructions.

use bot_core::{
    profile::{Adapter, Snapshot},
    state::{ConversationKind, RunContext},
};

const MAX_ATTACHMENTS_SHOWN: usize = 16;
const MAX_FIELD_CHARS: usize = 256;

fn clean(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(trimmed.len().min(MAX_FIELD_CHARS));
    for (count, ch) in trimmed.chars().enumerate() {
        if count >= MAX_FIELD_CHARS {
            out.push('…');
            break;
        }
        if ch.is_control() || matches!(ch, '\u{85}' | '\u{2028}' | '\u{2029}') {
            out.push('?');
        } else {
            out.push(ch);
        }
    }
    out
}

fn push(lines: &mut Vec<String>, key: &str, value: &str) {
    let value = clean(value);
    if !value.is_empty() {
        lines.push(format!("- {key}: {value}"));
    }
}

fn slack_workspace<'a>(snapshot: &'a Snapshot, run: &'a RunContext) -> Option<&'a str> {
    match snapshot.bindings.get(&run.binding).map(|b| &b.source) {
        Some(Adapter::SlackChannel { workspace, .. } | Adapter::SlackDm { workspace }) => {
            Some(workspace.as_str())
        }
        _ => run
            .principal
            .0
            .strip_prefix("slack:")
            .and_then(|rest| rest.split_once('/'))
            .map(|(workspace, _)| workspace),
    }
}

fn push_attachments(lines: &mut Vec<String>, attachments: &[String]) {
    let shown: Vec<String> = attachments
        .iter()
        .take(MAX_ATTACHMENTS_SHOWN)
        .map(|item| clean(item))
        .filter(|item| !item.is_empty())
        .collect();
    if shown.is_empty() {
        return;
    }
    let extra = attachments.len().saturating_sub(MAX_ATTACHMENTS_SHOWN);
    if extra > 0 {
        lines.push(format!(
            "- attachments: {} (+{extra} more)",
            shown.join(", ")
        ));
    } else {
        lines.push(format!("- attachments: {}", shown.join(", ")));
    }
}

/// Renders `## Request Context` tailored to the authenticated intake channel and origin.
pub(super) fn section(snapshot: &Snapshot, run: &RunContext) -> String {
    let mut lines = vec!["## Request Context".to_owned()];
    match &run.conversation.kind {
        ConversationKind::Channel => {
            push(&mut lines, "channel", "slack-channel");
            push(&mut lines, "origin", run.origin.as_str());
            if let Some(ws) = slack_workspace(snapshot, run) {
                push(&mut lines, "workspace", ws);
            }
            push(&mut lines, "binding", &run.binding.0);
            if let Some((team, channel)) = run.conversation.id.split_once('/') {
                push(&mut lines, "team_id", team);
                push(&mut lines, "channel_id", channel);
            } else {
                push(&mut lines, "channel_id", &run.conversation.id);
            }
            if let Some(thread) = &run.conversation.thread {
                push(&mut lines, "thread_ts", thread);
            }
            if let Some(ts) = &run.intake.message_ts {
                push(&mut lines, "message_ts", ts);
            }
            push(&mut lines, "requester", &run.principal.0);
            push_attachments(&mut lines, &run.work.attachments);
        }
        ConversationKind::Direct => {
            let telegram_bot = match snapshot.bindings.get(&run.binding).map(|b| &b.source) {
                Some(Adapter::TelegramDm { bot }) => Some(bot.as_str()),
                _ => None,
            };
            if let Some(bot) = telegram_bot {
                push(&mut lines, "channel", "telegram-dm");
                push(&mut lines, "origin", run.origin.as_str());
                push(&mut lines, "bot", bot);
                push(&mut lines, "binding", &run.binding.0);
                let chat_id = run
                    .conversation
                    .id
                    .split_once('/')
                    .map_or(run.conversation.id.as_str(), |(_, chat)| chat);
                push(&mut lines, "chat_id", chat_id);
                push(&mut lines, "requester", &run.principal.0);
                push_attachments(&mut lines, &run.work.attachments);
            } else {
                push(&mut lines, "channel", "slack-dm");
                push(&mut lines, "origin", run.origin.as_str());
                if let Some(ws) = slack_workspace(snapshot, run) {
                    push(&mut lines, "workspace", ws);
                }
                push(&mut lines, "binding", &run.binding.0);
                push(&mut lines, "channel_id", &run.conversation.id);
                if let Some(thread) = &run.conversation.thread {
                    push(&mut lines, "thread_ts", thread);
                }
                if let Some(ts) = &run.intake.message_ts {
                    push(&mut lines, "message_ts", ts);
                }
                push(&mut lines, "requester", &run.principal.0);
                push_attachments(&mut lines, &run.work.attachments);
            }
        }
        ConversationKind::Cli => {
            push(&mut lines, "channel", "cli");
            push(&mut lines, "origin", run.origin.as_str());
            push(&mut lines, "binding", &run.binding.0);
            push(&mut lines, "account", &run.conversation.id);
            if let Some(thread) = &run.conversation.thread {
                push(&mut lines, "thread", thread);
            }
            push(&mut lines, "requester", &run.principal.0);
            push_attachments(&mut lines, &run.work.attachments);
        }
        ConversationKind::Http => {
            if let Some(api) = &run.api {
                push(&mut lines, "channel", "openai-api");
                push(&mut lines, "origin", run.origin.as_str());
                push(&mut lines, "binding", &run.binding.0);
                push(&mut lines, "endpoint", &api.endpoint);
                push(&mut lines, "model", &api.model);
                push(&mut lines, "requester", &run.principal.0);
            } else {
                push(&mut lines, "channel", "http-webhook");
                push(&mut lines, "origin", run.origin.as_str());
                push(&mut lines, "binding", &run.binding.0);
                push(&mut lines, "endpoint", &run.conversation.id);
                if let Some((method, path)) = &run.intake.http_route {
                    push(&mut lines, "route", &format!("{method} {path}"));
                }
                if let Some(request_id) = &run.conversation.thread {
                    push(&mut lines, "request_id", request_id);
                }
                push(&mut lines, "requester", &run.principal.0);
                push_attachments(&mut lines, &run.work.attachments);
            }
        }
        ConversationKind::Scheduled => {
            push(&mut lines, "channel", "scheduled");
            push(&mut lines, "origin", run.origin.as_str());
            let trigger = run
                .trigger
                .as_ref()
                .map_or(run.conversation.id.as_str(), |t| t.id.as_str());
            push(&mut lines, "trigger", trigger);
            push(&mut lines, "requester", &run.principal.0);
        }
    }
    lines.join("\n")
}
