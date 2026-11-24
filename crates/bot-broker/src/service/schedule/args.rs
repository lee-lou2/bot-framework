//! 내장 예약 도구의 인자 읽기와 결과 모양(순수 함수). 인자는 엄격히 읽는다(중복 키·모르는 키·두 가지 시간 설정·과대 지시문 거절).
//!
//! `schedule.create`는 구조를 읽어 `ScheduleSpec`을 만든 뒤 core의 `ScheduleSpec::seal_create`로 봉인한다(카드·해시·저장이 같은 값을
//! 쓴다). 생략한 시간대(`cron`에 `timezone`이 없을 때)는 **봉인 전에** 실행이 고정한 판본의 프로필 시간대로 채우므로, 승인 뒤에 현재
//! 판본의 기본 시간대가 바뀌어도 승인한 카드와 다른 시간으로 저장되지 않는다. 봉인은 현재 시각을 읽지 않는다 — 이미 끝낸 호출의
//! 재전달도 같은 해시를 만든다(지나간 시각 검사는 새 호출에만 한다).

use crate::{
    canon::{canonicalize, sha256},
    json::parse,
    ports::Sealed,
    rfc3339::{format_utc, parse_utc},
};
use bot_core::{
    command::JsonPayload,
    cron::Cron,
    id::{BindingId, ScheduleId},
    port::Error,
    schedule::{DeliveryTarget, ScheduleSpec, ScheduleSummary, Timing},
    state::{Conversation, ConversationKind},
};
use serde_json::{Map, Value};
use std::time::SystemTime;

/// 인자를 거절한 이유.
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

/// 구조·문법까지 읽은 `schedule.create` 명세(시간에 기대는 검사는 아직 하지 않았다). 시간대는 이미 채워져 있다. `requested`는 모델이 지정한 다른
/// 수신자(없으면 원래 대화)이고, 호스트 등록부가 정규 경로로 확정한 값으로만 명세에 들어간다(`seal`).
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

/// `{"binding": "<바인딩>", "conversation": {"kind": "direct"|"channel", "id": "<수신 ID>", "thread": "<스레드>"|null}}`. 모르는 키·다른 종류는 거절한다.
/// 수신자의 이름·ID가 쓸 수 있는 글인지는 봉인(`seal_create`)이 가린다.
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
    /// `default_timezone`은 실행이 고정한 판본의 프로필 시간대다. `cron`에 `timezone`이 없으면 이 값으로 채운 명세를 core가 봉인한다.
    /// 시간대 이름·cron 문법·`at` 표현·지시문 한도·수신자의 모양은 core가 같은 규칙으로 거절한다(등록부를 묻기 전에 걸러진다).
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
        // 시간 설정은 하나만(`at` 한 번 또는 `cron` 반복)이고 시간대는 `cron`과만 쓴다.
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
        // 명세와 지정한 수신자의 모양이 봉인할 수 있는지 먼저 본다(틀린 인자는 등록부를 묻지 않고 거절한다).
        spec.seal_create().map_err(|_| Reject::Invalid)?;
        if let Some(target) = &requested {
            let mut trial = spec.clone();
            trial.destination = Some(target.clone());
            trial.seal_create().map_err(|_| Reject::Invalid)?;
        }
        Ok(Self { spec, requested })
    }

    /// 모델이 지정한 다른 수신자(없으면 원래 대화).
    pub(super) fn requested(&self) -> Option<&DeliveryTarget> {
        self.requested.as_ref()
    }

    /// 등록부가 정규 경로로 확정한 수신자(없으면 원래 대화)를 명세에 넣고 봉인한다. 카드·해시·저장이 모두 이 값을 쓴다.
    pub(super) fn seal(&mut self, resolved: Option<DeliveryTarget>) -> Result<Sealed, Reject> {
        self.spec.destination = resolved;
        let sealed = self.spec.seal_create().map_err(|_| Reject::Invalid)?;
        Ok(Sealed {
            canonical: sealed.canonical,
            sha256: sealed.sha256,
            destination: None,
        })
    }

    /// 새 호출의 시간 검사(`at`은 지금보다 뒤)를 마치고 저장소에 줄 명세를 돌려준다. 이미 끝낸 호출의 재전달에는 부르지 않는다.
    pub(super) fn spec(self, now: SystemTime) -> Result<ScheduleSpec, Error> {
        match self.spec.timing {
            Timing::At(at) if at <= now => Err(Error::Invalid), // 지나간 시각
            _ => Ok(self.spec),
        }
    }
}

/// 봉인을 core로 옮기기 전(구판)의 `schedule.create` 인자 지문: 시간대를 채운 인자 객체의 정규 JSON의 SHA-256(`quiet`·`at` 표기는
/// 모델이 쓴 그대로였다). **이미 끝난 호출의 재전달에서 저장된 영수증을 읽는 데만** 쓴다 — 새 호출·열린 승인·승인된 승인에는 쓰지 않는다.
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

/// `{"id": "<예약 ID>"}`만 받는다.
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

/// 목록의 인자는 비어 있어야 한다.
pub(super) fn parse_list(args: &JsonPayload) -> Result<Sealed, Reject> {
    match strict_object(args) {
        Some(map) if map.is_empty() => seal(map).ok_or(Reject::Invalid),
        _ => Err(Reject::Invalid),
    }
}

/// `{"schedules":[…]}`: 지시문·수신자·원래 대화는 싣지 않는다.
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
