//! Slack가 보낸 JSON을 읽어 우리 이벤트로 바꾼다(순수 함수). 받을지 말지는 판단하지 않고 모양만 읽는다.
//! Socket Mode 프레임과 Events API 본문(`event_callback`)은 안쪽 이벤트 모양이 같아 같은 해석기를 쓴다.
//! 워크스페이스(팀)는 봉투의 `team_id`/`team.id`만 믿는다. 이벤트 본문의 다른 조직 값은 보지 않는다.
use serde_json::Value;

/// 사람이 쓴 글 한 건(채널 멘션 또는 DM)
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Message {
    pub team: String,
    pub channel: String,
    /// 봇과의 1:1 DM(`channel_type: im`)
    pub direct: bool,
    pub user: String,
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    /// 첨부 파일 ID(내려받지 않는다)
    pub files: Vec<String>,
    /// 이 이벤트를 전한 Slack 이벤트 ID(재전달에 같다)
    pub event_id: Option<String>,
}

/// 채널(공개 `message.channels`·비공개 `message.groups`)의 글 한 건. 사람의 글도 봇의 글도 담고, 무엇으로 받을지(스레드 답글인지)·자기 봇의 글인지는
/// 설치를 아는 처리기가 정한다. 수정·삭제·참여 알림 같은 시스템 글은 담지 않는다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Post {
    pub team: String,
    pub channel: String,
    /// 보낸 사용자 ID. 앱·시스템 글에는 없을 수 있다.
    pub user: Option<String>,
    /// 사람이 쓴 글인가(앱·봇 글이 아니다). 파일이 붙은 글·스레드 방송 글은 사람의 글이다.
    pub human: bool,
    pub text: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub files: Vec<String>,
    pub event_id: Option<String>,
}

/// 채널 글에 단 반응(`reaction_added`, `item.type = message`). 행위자는 `user`, 대상은 `channel`의 `item_ts` 글이다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reaction {
    pub team: String,
    pub channel: String,
    pub item_ts: String,
    pub user: String,
    pub reaction: String,
    /// 반응 대상 글의 작성자(행위자와 다를 수 있다)
    pub item_user: Option<String>,
    pub event_id: String,
}

/// 메시지 속 버튼 클릭
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Button {
    pub team: String,
    pub channel: String,
    pub user: String,
    pub message_ts: String,
    pub action_id: String,
    pub value: String,
}

/// Slack Agent 화면의 중단 버튼(`agent_session_stopped`)
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopRequested {
    pub team: String,
    pub channel: String,
    pub thread_ts: String,
    pub user: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Frame {
    /// 연결된 Slack 앱 ID(`connection_info.app_id`)
    Hello(Option<String>),
    Disconnect(String),
    Message(Message),
    Post(Post),
    Reaction(Reaction),
    Button(Button),
    StopRequested(StopRequested),
    /// 다루지 않는 이벤트(받았다는 표시만 한다)
    Ignored,
    Invalid,
}

/// 소켓 프레임 한 건과 그 ack에 쓸 envelope_id
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

/// 앱·봇이 보낸 글인가: `bot_id`·`app_id`가 있거나 `bot_message` 하위 유형이다(하나만 있어도 앱 글이다).
fn from_app(event: &Value) -> bool {
    event["bot_id"].is_string()
        || event["app_id"].is_string()
        || event["subtype"].as_str() == Some("bot_message")
}

/// `thread_ts`: 생략이면 `Ok(None)`, 비어 있지 않은 문자열이면 `Ok(Some)`. 그 밖(숫자·`null`·객체·빈 글)은 모양이 틀린 것이라 `Err` — 없는 것으로 보고 검사를 건너뛰지 않는다.
fn thread_of(event: &Value) -> Result<Option<String>, ()> {
    match event.get("thread_ts") {
        None => Ok(None),
        Some(Value::String(ts)) if !ts.is_empty() => Ok(Some(ts.clone())),
        Some(_) => Err(()),
    }
}

/// Events API 본문(`{team_id, event: {...}}`). 사람이 쓴 글만 `Message`가 된다.
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
    // 앱·봇이 보낸 글(자기 글 포함)은 대화 루프를 막으려고 받지 않는다
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
    // 글·스레드 ts는 채널 글과 같은 검사를 받는다: 잘못된 ts는 요청 ID·대화 주소로 쓰이기 전에 버린다
    let Ok(thread_ts) = thread_of(event) else {
        return Frame::Ignored;
    };
    if !ts_ok(&ts) || thread_ts.as_deref().is_some_and(|t| !ts_ok(t)) {
        return Frame::Ignored;
    }
    let direct = match kind {
        Some("message") => {
            // DM의 일반 글만. 수정·삭제·참여 알림 같은 시스템 글은 제외하고 파일이 붙은 글은 받는다
            if event["channel_type"].as_str() != Some("im") {
                return Frame::Ignored;
            }
            if !matches!(event["subtype"].as_str(), None | Some("file_share")) {
                return Frame::Ignored;
            }
            true
        }
        _ => {
            // 채널 멘션만. DM의 글은 `message`로 받으므로 중복 접수를 막으려고 건너뛴다
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

/// Slack ts 모양(`초.마이크로초`)인가. 잘못된 ts는 작업을 만들지 않는다.
fn ts_ok(ts: &str) -> bool {
    crate::schedule_routes::is_ts(ts)
}

/// 채널 글. 일반 글·파일 글·스레드 방송 글과 앱 글(`bot_message`)만 담고(자기 봇 글을 증거로 읽으려면 앱 글도 필요하다) 수정·삭제 같은 나머지는 버린다.
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

/// 반응 이름: 소문자·숫자와 `_`·`+`·`-`·`:`만, 100자 이하(`white_check_mark`·`+1`·`thumbsup::skin-tone-2`). Slack의 이모지 이름 규칙이고, 그 밖의 글자가 든 이름은 받지 않는다
/// (이름은 트리거 조건·기본 에이전트 입력에 그대로 실린다).
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
    let (Some(team), Some(channel), Some(thread_ts)) = (
        text(&payload["team_id"]),
        text(&event["channel"]).or_else(|| text(&event["channel_id"])),
        text(&event["thread_ts"]),
    ) else {
        return Frame::Ignored;
    };
    if !ts_ok(&thread_ts) {
        return Frame::Ignored;
    }
    let user = text(&event["user"])
        .or_else(|| text(&event["user_id"]))
        .unwrap_or_default();
    Frame::StopRequested(StopRequested {
        team,
        channel,
        thread_ts,
        user,
    })
}

/// 인터랙티브 본문. 버튼 클릭(`block_actions`)만 읽는다. 앱이 설치된 워크스페이스(`team.id`)만 쓰고,
/// 누른 사람의 소속(`user.team_id`)은 다를 수 있어 쓰지 않는다.
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

/// 재접속 지연(ms): 1초에서 두 배씩, 최대 30초
pub fn reconnect_delay_ms(attempt: u32) -> u64 {
    1000u64
        .saturating_mul(2u64.saturating_pow(attempt.max(1) - 1))
        .min(30_000)
}

#[cfg(test)]
mod tests;
