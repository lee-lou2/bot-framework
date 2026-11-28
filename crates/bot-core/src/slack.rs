//! Slack event selection, authenticated event sources, and bot thread participation records.
use crate::{
    id::{BindingId, ProfileId},
    port::Reply,
};
use std::collections::BTreeSet;

/// Slack event types configured under `events` in `profile.yaml` (`mention`, `thread_reply`, `reaction` for channels; `dm` for DMs).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum SlackEvent {
    Mention,
    Dm,
    ThreadReply,
    Reaction,
}

impl SlackEvent {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mention => "mention",
            Self::Dm => "dm",
            Self::ThreadReply => "thread_reply",
            Self::Reaction => "reaction",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "mention" => Self::Mention,
            "dm" => Self::Dm,
            "thread_reply" => Self::ThreadReply,
            "reaction" => Self::Reaction,
            _ => return None,
        })
    }

    /// Matching trigger `on.event` name; both `Mention` and `Dm` map to `"message"`.
    pub const fn trigger_event(self) -> &'static str {
        match self {
            Self::Mention | Self::Dm => "message",
            Self::ThreadReply => "thread-reply",
            Self::Reaction => "reaction",
        }
    }
}

/// Default for channel bindings when `events` is omitted: mentions only.
pub fn default_channel_events() -> BTreeSet<SlackEvent> {
    BTreeSet::from([SlackEvent::Mention])
}

/// Normalized intake event kind matching trigger `on.event` (`Message` covers mention and DM).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SourceKind {
    Message,
    ThreadReply,
    Reaction,
}

/// Event source read directly from an authenticated Slack payload and passed only via `AcceptExtras`; never constructed from model, tool, or message body input. Actor identity lives in `Accept.principal`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlackSource {
    pub kind: SourceKind,
    pub team: String,
    pub channel: String,
    /// Message timestamp; for reactions, the target item's `item.ts`.
    pub ts: String,
    /// Thread root timestamp when the event is a reply.
    pub thread_ts: Option<String>,
    /// Slack delivery event ID; identical across retries, though one post can arrive as both `app_mention` and `message` with distinct IDs.
    pub event_id: String,
    pub reaction: Option<String>,
}

impl SlackSource {
    /// Canonical deduplication key shared across deliveries, bindings, and profiles: `(team, channel, ts)` for posts (collapsing `app_mention` + `message` duplicates) and `(team, event_id)` for reactions. Uses `U+001F` as separator because identifiers forbid control characters, preventing boundary collisions.
    pub fn event_key(&self) -> String {
        match self.kind {
            SourceKind::Message | SourceKind::ThreadReply => {
                format!(
                    "m\u{1f}{}\u{1f}{}\u{1f}{}",
                    self.team, self.channel, self.ts
                )
            }
            SourceKind::Reaction => format!("r\u{1f}{}\u{1f}{}", self.team, self.event_id),
        }
    }
}

/// Extracts the triggering Slack message `ts` from a stored `slack_key` (returns `None` for reactions).
pub fn message_ts_from_key(slack_key: &str) -> Option<String> {
    let mut parts = slack_key.split('\u{1f}');
    match parts.next()? {
        "m" => {
            let (team, channel, ts) = (parts.next()?, parts.next()?, parts.next()?);
            if parts.next().is_some() || team.is_empty() || channel.is_empty() || ts.is_empty() {
                return None;
            }
            Some(ts.to_owned())
        }
        "l" => {
            let raw = parts.next()?;
            if parts.next().is_some() {
                return None;
            }
            let (channel, ts) = raw.split_once('.')?;
            (!channel.is_empty() && !ts.is_empty()).then(|| ts.to_owned())
        }
        _ => None,
    }
}

/// Proof that a specific bot installation participated in a thread; changing team or bot creates a distinct ownership key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BotThread {
    pub profile: ProfileId,
    pub binding: BindingId,
    pub team: String,
    /// Bot user ID from `auth.test`.
    pub bot: String,
    pub channel: String,
    pub root_ts: String,
}

/// Backing store proving bot thread participation before accepting `thread_reply` events; incoming `thread_ts` alone is never trusted.
pub trait BotThreads: Send + Sync {
    /// Idempotently records a thread from a confirmed post receipt or authenticated self-bot message.
    fn note(&self, thread: BotThread) -> Reply<'_, ()>;
    /// Returns `Err(Unavailable)` on store error so callers fail closed instead of admitting unverified threads.
    fn known(&self, thread: BotThread) -> Reply<'_, bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_post_is_one_event_whatever_delivery_it_came_by_and_a_reaction_is_its_own_event() {
        let source = |kind, event_id: &str| SlackSource {
            kind,
            team: "T1".into(),
            channel: "C1".into(),
            ts: "100.1".into(),
            thread_ts: None,
            event_id: event_id.into(),
            reaction: None,
        };
        let mention = source(SourceKind::Message, "Ev1");
        assert_eq!(
            mention.event_key(),
            source(SourceKind::Message, "Ev2").event_key()
        );
        assert_eq!(
            mention.event_key(),
            source(SourceKind::ThreadReply, "Ev3").event_key()
        );
        let reaction = source(SourceKind::Reaction, "Ev9");
        assert_ne!(reaction.event_key(), mention.event_key());
        assert_ne!(
            reaction.event_key(),
            source(SourceKind::Reaction, "Ev8").event_key()
        );
        assert_eq!(
            reaction.event_key(),
            source(SourceKind::Reaction, "Ev9").event_key()
        );
        for other in [
            SlackSource {
                team: "T2".into(),
                ..mention.clone()
            },
            SlackSource {
                channel: "C2".into(),
                ..mention.clone()
            },
            SlackSource {
                ts: "100.2".into(),
                ..mention.clone()
            },
        ] {
            assert_ne!(other.event_key(), mention.event_key());
        }
    }

    #[test]
    fn event_names_round_trip_and_map_to_trigger_events() {
        for event in [
            SlackEvent::Mention,
            SlackEvent::Dm,
            SlackEvent::ThreadReply,
            SlackEvent::Reaction,
        ] {
            assert_eq!(SlackEvent::parse(event.as_str()), Some(event));
        }
        assert_eq!(SlackEvent::parse("message"), None);
        assert_eq!(SlackEvent::parse("thread-reply"), None);
        assert_eq!(SlackEvent::Mention.trigger_event(), "message");
        assert_eq!(SlackEvent::Dm.trigger_event(), "message");
        assert_eq!(SlackEvent::ThreadReply.trigger_event(), "thread-reply");
        assert_eq!(SlackEvent::Reaction.trigger_event(), "reaction");
        assert_eq!(default_channel_events().len(), 1);
    }

    #[test]
    fn message_ts_from_key_extracts_message_and_legacy_timestamps_only() {
        assert_eq!(
            message_ts_from_key("m\u{1f}T1\u{1f}C1\u{1f}1700000000.000100").as_deref(),
            Some("1700000000.000100")
        );
        assert_eq!(
            message_ts_from_key("l\u{1f}C1.1700000000.000100").as_deref(),
            Some("1700000000.000100")
        );
        for non_message in [
            "r\u{1f}T1\u{1f}Ev1",
            "",
            "m\u{1f}T1\u{1f}C1",
            "m\u{1f}T1\u{1f}C1\u{1f}",
            "m\u{1f}T1\u{1f}C1\u{1f}1.2\u{1f}extra",
            "l\u{1f}C1",
            "l\u{1f}.1.2",
            "l\u{1f}C1.",
            "l\u{1f}C1.1.2\u{1f}extra",
        ] {
            assert_eq!(message_ts_from_key(non_message), None, "{non_message:?}");
        }
    }
}
