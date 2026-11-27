//! Pure Slack Socket Mode and Events API frame parser.
//!
//! Trusts only the envelope `team_id` / `team.id` (never inner organization fields), rejects malformed timestamps
//! or emoji names, and filters app/bot messages on DM/mention paths to prevent reply loops.
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub team: String,
    pub channel: String,
    pub direct: bool,
    pub user: String,
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    /// Attachment file IDs only; file contents are never downloaded.
    pub files: Vec<String>,
    pub event_id: Option<String>,
}

/// Channel post (`message.channels` / `message.groups`) including bot posts so handlers can verify own-bot thread evidence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Post {
    pub team: String,
    pub channel: String,
    pub user: Option<String>,
    pub human: bool,
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub files: Vec<String>,
    pub event_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reaction {
    pub team: String,
    pub channel: String,
    pub item_ts: String,
    pub user: String,
    pub reaction: String,
    pub item_user: Option<String>,
    pub event_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Button {
    pub team: String,
    pub channel: String,
    pub user: String,
    pub message_ts: String,
    pub action_id: String,
    pub value: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopRequested {
    pub team: String,
    pub channel: String,
    pub thread_ts: String,
    pub user: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Frame {
    Hello(Option<String>),
    Disconnect(String),
    Message(Message),
    Post(Post),
    Reaction(Reaction),
    Button(Button),
    StopRequested(StopRequested),
    Ignored,
    Invalid,
}

pub fn parse_socket(raw: &str) -> (Frame, Option<String>) {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return (Frame::Invalid, None);
    };
    let ack = text(&value["envelope_id"]);
    let frame = match value["type"].as_str() {
        Some("hello") => Frame::Hello(text(&value["connection_info"]["app_id"])),
        Some("disconnect") => {
            Frame::Disconnect(value["reason"].as_str().unwrap_or("unknown").to_owned())
        }
        Some("events_api") => parse_event(&value["payload"]),
        Some("interactive") => parse_interactive(&value["payload"]),
        _ => Frame::Ignored,
    };
    (frame, ack)
}

fn text(value: &Value) -> Option<String> {
    value.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn from_app(event: &Value) -> bool {
    event["bot_id"].is_string()
        || event["app_id"].is_string()
        || event["subtype"].as_str() == Some("bot_message")
}

/// Rejects malformed non-string or empty `thread_ts` instead of treating them as absent.
fn thread_of(event: &Value) -> Result<Option<String>, ()> {
    match event.get("thread_ts") {
        None => Ok(None),
        Some(Value::String(ts)) if !ts.is_empty() => Ok(Some(ts.clone())),
        Some(_) => Err(()),
    }
}

pub fn parse_event(payload: &Value) -> Frame {
    let event = &payload["event"];
    let kind = event["type"].as_str();
    if kind == Some("reaction_added") {
        return parse_reaction(payload);
    }
    if kind == Some("agent_session_stopped") {
        return parse_stop_requested(payload);
    }
    if !matches!(kind, Some("app_mention" | "message")) {
        return Frame::Ignored;
    }
    if kind == Some("message")
        && matches!(event["channel_type"].as_str(), Some("channel" | "group"))
    {
        return parse_post(payload);
    }
    if from_app(event) {
        return Frame::Ignored;
    }
    let (Some(team), Some(channel), Some(user), Some(ts)) = (
        text(&payload["team_id"]),
        text(&event["channel"]),
        text(&event["user"]),
        text(&event["ts"]),
    ) else {
        return Frame::Ignored;
    };
    let Ok(thread_ts) = thread_of(event) else {
        return Frame::Ignored;
    };
    if !ts_ok(&ts) || thread_ts.as_deref().is_some_and(|t| !ts_ok(t)) {
        return Frame::Ignored;
    }
    let direct = match kind {
        Some("message") => {
            if event["channel_type"].as_str() != Some("im") {
                return Frame::Ignored;
            }
            if !matches!(event["subtype"].as_str(), None | Some("file_share")) {
                return Frame::Ignored;
            }
            true
        }
        _ => {
            // Skip `app_mention` in DMs because DMs already arrive via `message`.
            if channel.starts_with('D') {
                return Frame::Ignored;
            }
            false
        }
    };
    Frame::Message(Message {
        team,
        channel,
        direct,
        user,
        text: event["text"].as_str().unwrap_or_default().to_owned(),
        ts,
        thread_ts,
        files: file_ids(event),
        event_id: text(&payload["event_id"]),
    })
}

fn file_ids(event: &Value) -> Vec<String> {
    event["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| text(&file["id"]))
        .collect()
}

fn ts_ok(ts: &str) -> bool {
    crate::schedule_routes::is_ts(ts)
}

fn parse_post(payload: &Value) -> Frame {
    let event = &payload["event"];
    let subtype = event["subtype"].as_str();
    if !matches!(
        subtype,
        None | Some("file_share" | "thread_broadcast" | "bot_message")
    ) {
        return Frame::Ignored;
    }
    let (Some(team), Some(channel), Some(ts)) = (
        text(&payload["team_id"]),
        text(&event["channel"]),
        text(&event["ts"]),
    ) else {
        return Frame::Ignored;
    };
    let Ok(thread_ts) = thread_of(event) else {
        return Frame::Ignored;
    };
    if !ts_ok(&ts) || thread_ts.as_deref().is_some_and(|t| !ts_ok(t)) {
        return Frame::Ignored;
    }
    let user = text(&event["user"]);
    let app = from_app(event);
    Frame::Post(Post {
        team,
        channel,
        human: !app && user.is_some(),
        user,
        text: event["text"].as_str().unwrap_or_default().to_owned(),
        ts,
        thread_ts,
        files: file_ids(event),
        event_id: text(&payload["event_id"]),
    })
}

fn reaction_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'+' | b'-' | b':')
        })
}

fn parse_reaction(payload: &Value) -> Frame {
    let event = &payload["event"];
    let item = &event["item"];
    if item["type"].as_str() != Some("message") {
        return Frame::Ignored;
    }
    let (Some(team), Some(channel), Some(item_ts), Some(user), Some(reaction), Some(event_id)) = (
        text(&payload["team_id"]),
        text(&item["channel"]),
        text(&item["ts"]),
        text(&event["user"]),
        text(&event["reaction"]),
        text(&payload["event_id"]),
    ) else {
        return Frame::Ignored;
    };
    if !ts_ok(&item_ts) || !reaction_ok(&reaction) {
        return Frame::Ignored;
    }
    Frame::Reaction(Reaction {
        team,
        channel,
        item_ts,
        user,
        reaction,
        item_user: text(&event["item_user"]),
        event_id,
    })
}

fn parse_stop_requested(payload: &Value) -> Frame {
    let event = &payload["event"];
    let (Some(team), Some(channel), Some(thread_ts), Some(user)) = (
        text(&payload["team_id"]),
        text(&event["channel"]).or_else(|| text(&event["channel_id"])),
        text(&event["thread_ts"]),
        text(&event["user"]).or_else(|| text(&event["user_id"])),
    ) else {
        return Frame::Ignored;
    };
    if !ts_ok(&thread_ts) {
        return Frame::Ignored;
    }
    Frame::StopRequested(StopRequested {
        team,
        channel,
        thread_ts,
        user,
    })
}

/// Parses `block_actions` using only the installed workspace `team.id` (never `user.team_id`).
pub fn parse_interactive(payload: &Value) -> Frame {
    if payload["type"].as_str() != Some("block_actions") {
        return Frame::Ignored;
    }
    let action = &payload["actions"][0];
    let container = &payload["container"];
    let pick = |a: &Value, b: &Value| text(a).or_else(|| text(b));
    let (Some(team), Some(user), Some(channel), Some(action_id)) = (
        text(&payload["team"]["id"]),
        text(&payload["user"]["id"]),
        pick(&payload["channel"]["id"], &container["channel_id"]),
        text(&action["action_id"]),
    ) else {
        return Frame::Ignored;
    };
    Frame::Button(Button {
        team,
        channel,
        user,
        message_ts: pick(&payload["message"]["ts"], &container["message_ts"]).unwrap_or_default(),
        action_id,
        value: action["value"].as_str().unwrap_or_default().to_owned(),
    })
}

/// Exponential reconnect backoff starting at 1 s, capped at 30 s.
pub fn reconnect_delay_ms(attempt: u32) -> u64 {
    1000u64
        .saturating_mul(2u64.saturating_pow(attempt.max(1) - 1))
        .min(30_000)
}

#[cfg(test)]
mod tests;
