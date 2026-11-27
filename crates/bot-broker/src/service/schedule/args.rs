//! Pure argument parsing, sealing, and list rendering for built-in schedule tools.
//!
//! `schedule.create` fills any omitted cron `timezone` from the run's pinned revision before
//! calling `ScheduleSpec::seal_create`, so card, hash, and persisted spec stay identical across
//! default-timezone changes without depending on wall-clock time (keeping replays deterministic).

use crate::{
    canon::{canonicalize, sha256},
    json::parse,
    ports::Sealed,
};
use bot_core::{
    builtin::MAX_SCHEDULE_AHEAD_MS,
    command::JsonPayload,
    cron::Cron,
    id::{BindingId, ScheduleId},
    port::Error,
    schedule::{
        DeliveryTarget, ScheduleSpec, ScheduleSummary, Timing,
        utc::{format_utc, parse_utc},
    },
    state::{Conversation, ConversationKind},
};
use serde_json::{Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Eq, PartialEq)]
pub(super) enum Reject {
    Invalid,
}

pub(super) fn strict_object(args: &JsonPayload) -> Option<Map<String, Value>> {
    match parse(&args.0)? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

pub(super) fn seal(map: Map<String, Value>) -> Option<Sealed> {
    let canonical = canonicalize(&Value::Object(map))?;
    Some(Sealed {
        sha256: sha256(&canonical),
        canonical,
        destination: None,
    })
}

/// Structurally validated `schedule.create` request prior to destination canonicalization and time-range checks.
pub(super) struct Create {
    spec: ScheduleSpec,
    requested: Option<DeliveryTarget>,
}

fn only_keys(map: &Map<String, Value>, allowed: &[&str]) -> bool {
    map.keys().all(|key| allowed.contains(&key.as_str()))
}

fn text_of<'a>(map: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, Reject> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(Reject::Invalid),
    }
}

fn destination_of(value: &Value) -> Result<DeliveryTarget, Reject> {
    let map = value.as_object().ok_or(Reject::Invalid)?;
    let conversation = match (map.len(), map.get("conversation")) {
        (2, Some(Value::Object(conversation))) => conversation,
        _ => return Err(Reject::Invalid),
    };
    if !only_keys(conversation, &["kind", "id", "thread"]) {
        return Err(Reject::Invalid);
    }
    let kind = match text_of(conversation, "kind")? {
        Some("direct") => ConversationKind::Direct,
        Some("channel") => ConversationKind::Channel,
        _ => return Err(Reject::Invalid),
    };
    let thread = match conversation.get("thread") {
        None | Some(Value::Null) => None,
        Some(Value::String(thread)) => Some(thread.clone()),
        Some(_) => return Err(Reject::Invalid),
    };
    Ok(DeliveryTarget {
        binding: BindingId(text_of(map, "binding")?.ok_or(Reject::Invalid)?.to_owned()),
        conversation: Conversation {
            kind,
            id: text_of(conversation, "id")?
                .ok_or(Reject::Invalid)?
                .to_owned(),
            thread,
        },
    })
}

impl Create {
    pub(super) fn parse(args: &JsonPayload, default_timezone: &str) -> Result<Self, Reject> {
        let map = strict_object(args).ok_or(Reject::Invalid)?;
        if !only_keys(
            &map,
            &[
                "at",
                "cron",
                "timezone",
                "instruction",
                "quiet",
                "destination",
            ],
        ) {
            return Err(Reject::Invalid);
        }
        let requested = map.get("destination").map(destination_of).transpose()?;
        let (at, cron, zone) = (
            text_of(&map, "at")?,
            text_of(&map, "cron")?,
            text_of(&map, "timezone")?,
        );
        if at.is_some() == cron.is_some() || (zone.is_some() && cron.is_none()) {
            return Err(Reject::Invalid);
        }
        let instruction = text_of(&map, "instruction")?.ok_or(Reject::Invalid)?;
        let quiet = match map.get("quiet") {
            None => false,
            Some(Value::Bool(quiet)) => *quiet,
            Some(_) => return Err(Reject::Invalid),
        };
        let timing = match (at, cron) {
            (Some(at), None) => Timing::At(parse_utc(at).ok_or(Reject::Invalid)?),
            (None, Some(cron)) => Timing::Cron {
                expression: Cron::parse(cron).map_err(|_| Reject::Invalid)?,
                timezone: zone.unwrap_or(default_timezone).to_owned(),
            },
            _ => return Err(Reject::Invalid),
        };
        let spec = ScheduleSpec {
            timing,
            instruction: instruction.to_owned(),
            quiet,
            destination: None,
        };
        // Validate spec and destination shape via core sealing before querying the route registry.
        spec.seal_create().map_err(|_| Reject::Invalid)?;
        if let Some(target) = &requested {
            let mut trial = spec.clone();
            trial.destination = Some(target.clone());
            trial.seal_create().map_err(|_| Reject::Invalid)?;
        }
        Ok(Self { spec, requested })
    }

    pub(super) fn requested(&self) -> Option<&DeliveryTarget> {
        self.requested.as_ref()
    }

    /// Binds the canonical registry-resolved destination (or `None` for the originating conversation) and seals it.
    pub(super) fn seal(&mut self, resolved: Option<DeliveryTarget>) -> Result<Sealed, Reject> {
        self.spec.destination = resolved;
        let sealed = self.spec.seal_create().map_err(|_| Reject::Invalid)?;
        Ok(Sealed {
            canonical: sealed.canonical,
            sha256: sealed.sha256,
            destination: None,
        })
    }

    /// Enforces `at` bounds against `now` for new calls only (skipped on completed-call replays).
    pub(super) fn spec(self, now: SystemTime) -> Result<ScheduleSpec, Error> {
        if let Timing::At(at) = self.spec.timing {
            let millis = |time: SystemTime| match time.duration_since(UNIX_EPOCH) {
                Ok(later) => later.as_millis() as i128,
                Err(earlier) => -(earlier.duration().as_millis() as i128),
            };
            let ahead = millis(at) - millis(now);
            if ahead <= 0 || ahead > i128::from(MAX_SCHEDULE_AHEAD_MS) {
                return Err(Error::Invalid);
            }
        }
        Ok(self.spec)
    }
}

/// Pre-core legacy `schedule.create` SHA-256 fingerprint, used solely to replay already-completed receipts.
pub(super) fn legacy_create_sha256(args: &JsonPayload, default_timezone: &str) -> Option<[u8; 32]> {
    let mut map = strict_object(args)?;
    if map.contains_key("cron") && !map.contains_key("timezone") {
        map.insert(
            "timezone".into(),
            Value::String(default_timezone.to_owned()),
        );
    }
    seal(map).map(|sealed| sealed.sha256)
}

pub(super) fn parse_cancel(args: &JsonPayload) -> Result<(ScheduleId, Sealed), Reject> {
    let map = strict_object(args).ok_or(Reject::Invalid)?;
    let id = match (map.len(), map.get("id")) {
        (1, Some(Value::String(id)))
            if !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            ScheduleId(id.clone())
        }
        _ => return Err(Reject::Invalid),
    };
    let sealed = seal(map).ok_or(Reject::Invalid)?;
    Ok((id, sealed))
}

pub(super) fn parse_list(args: &JsonPayload) -> Result<Sealed, Reject> {
    match strict_object(args) {
        Some(map) if map.is_empty() => seal(map).ok_or(Reject::Invalid),
        _ => Err(Reject::Invalid),
    }
}

/// Renders `{"schedules":[...]}` omitting instructions, destinations, and originating conversations.
pub(super) fn render_list(list: &[ScheduleSummary]) -> Option<JsonPayload> {
    let items: Vec<Value> = list
        .iter()
        .map(|item| {
            let mut entry = Map::new();
            entry.insert("id".into(), item.id.0.clone().into());
            entry.insert("quiet".into(), item.quiet.into());
            match &item.timing {
                Timing::At(at) => {
                    entry.insert("at".into(), format_utc(*at).into());
                }
                Timing::Cron {
                    expression,
                    timezone,
                } => {
                    entry.insert("cron".into(), expression.as_str().into());
                    entry.insert("timezone".into(), timezone.clone().into());
                }
            }
            Value::Object(entry)
        })
        .collect();
    let mut body = Map::new();
    body.insert("schedules".into(), Value::Array(items));
    canonicalize(&Value::Object(body)).map(JsonPayload)
}
