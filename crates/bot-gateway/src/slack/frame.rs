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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Frame {
    /// 연결된 Slack 앱 ID(`connection_info.app_id`)
    Hello(Option<String>),
    Disconnect(String),
    Message(Message),
    Button(Button),
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

/// Events API 본문(`{team_id, event: {...}}`). 사람이 쓴 글만 `Message`가 된다.
pub fn parse_event(payload: &Value) -> Frame {
    let event = &payload["event"];
    let kind = event["type"].as_str();
    if !matches!(kind, Some("app_mention" | "message")) {
        return Frame::Ignored;
    }
    // 앱·봇이 보낸 글(자기 글 포함)은 대화 루프를 막으려고 받지 않는다
    if event["bot_id"].is_string() || event["subtype"].as_str() == Some("bot_message") {
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
        thread_ts: text(&event["thread_ts"]),
        files: event["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|file| text(&file["id"]))
            .collect(),
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
mod tests {
    use super::*;

    fn events(event: &str) -> String {
        format!(
            r#"{{"type":"events_api","envelope_id":"E1","payload":{{"team_id":"T1","event_id":"Ev1","event":{event}}}}}"#
        )
    }

    fn message(frame: Frame) -> Message {
        match frame {
            Frame::Message(m) => m,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hello_disconnect_and_garbage() {
        assert_eq!(parse_socket(r#"{"type":"hello"}"#).0, Frame::Hello(None));
        assert_eq!(
            parse_socket(r#"{"type":"hello","connection_info":{"app_id":"A1"}}"#).0,
            Frame::Hello(Some("A1".into()))
        );
        let (frame, ack) = parse_socket(r#"{"type":"disconnect","reason":"refresh_requested"}"#);
        assert_eq!(
            (frame, ack),
            (Frame::Disconnect("refresh_requested".into()), None)
        );
        assert_eq!(parse_socket("not json").0, Frame::Invalid);
        assert_eq!(
            parse_socket(r#"{"type":"slash_commands"}"#).0,
            Frame::Ignored
        );
    }

    #[test]
    fn a_channel_mention_keeps_its_team_thread_and_files() {
        let raw = events(
            r#"{"type":"app_mention","user":"U1","text":"<@UB> 안녕","ts":"10.1","channel":"C1","files":[{"id":"F1"},{"id":"F2"},{"name":"no id"}]}"#,
        );
        let (frame, ack) = parse_socket(&raw);
        assert_eq!(ack.as_deref(), Some("E1"));
        let m = message(frame);
        assert_eq!(
            (m.team.as_str(), m.channel.as_str(), m.direct),
            ("T1", "C1", false)
        );
        assert_eq!(
            (m.user.as_str(), m.ts.as_str(), m.thread_ts),
            ("U1", "10.1", None)
        );
        assert_eq!(m.files, ["F1", "F2"]);
        let inside = events(
            r#"{"type":"app_mention","user":"U1","text":"x","ts":"11.1","thread_ts":"10.1","channel":"C1"}"#,
        );
        assert_eq!(
            message(parse_socket(&inside).0).thread_ts.as_deref(),
            Some("10.1")
        );
    }

    #[test]
    fn only_plain_human_dms_become_direct_messages() {
        let dm = events(
            r#"{"type":"message","channel_type":"im","user":"U1","text":"질문","ts":"1.1","channel":"D1"}"#,
        );
        let m = message(parse_socket(&dm).0);
        assert!(m.direct);
        assert_eq!(m.channel, "D1");
        let with_file = events(
            r#"{"type":"message","subtype":"file_share","channel_type":"im","user":"U1","ts":"1.2","channel":"D1","files":[{"id":"F9"}]}"#,
        );
        assert_eq!(message(parse_socket(&with_file).0).files, ["F9"]);
        for ignored in [
            // 수정·삭제 같은 시스템 글
            r#"{"type":"message","subtype":"message_changed","channel_type":"im","channel":"D1","user":"U1","ts":"1"}"#,
            // 채널의 일반 글(멘션이 아니면 받지 않는다)
            r#"{"type":"message","channel_type":"channel","user":"U1","text":"x","ts":"1.3","channel":"C1"}"#,
            // 앱·봇이 보낸 글
            r#"{"type":"message","channel_type":"im","user":"U9","bot_id":"B1","text":"x","ts":"1.4","channel":"D1"}"#,
            r#"{"type":"message","subtype":"bot_message","channel_type":"im","user":"U9","text":"x","ts":"1.5","channel":"D1"}"#,
            // 보낸 사람·글 ID를 알 수 없음
            r#"{"type":"message","channel_type":"im","text":"x","ts":"1.6","channel":"D1"}"#,
            // DM에서의 app_mention은 message로 받으므로 건너뛴다
            r#"{"type":"app_mention","user":"U1","text":"x","ts":"1.7","channel":"D1"}"#,
            r#"{"type":"reaction_added","user":"U1","reaction":"x"}"#,
        ] {
            assert_eq!(
                parse_socket(&events(ignored)).0,
                Frame::Ignored,
                "{ignored}"
            );
        }
    }

    #[test]
    fn events_without_a_team_are_ignored() {
        let raw = r#"{"type":"events_api","envelope_id":"E5","payload":{"event":{"type":"app_mention","user":"U1","text":"x","ts":"1.1","channel":"C1"}}}"#;
        let (frame, ack) = parse_socket(raw);
        assert_eq!((frame, ack.as_deref()), (Frame::Ignored, Some("E5")));
        let empty = r#"{"type":"events_api","envelope_id":"E6","payload":{"team_id":"","event":{"type":"app_mention","user":"U1","text":"x","ts":"1.1","channel":"C1"}}}"#;
        assert_eq!(parse_socket(empty).0, Frame::Ignored);
    }

    #[test]
    fn button_clicks_use_the_installation_team_not_the_clickers() {
        let raw = r#"{"type":"interactive","envelope_id":"E2","payload":{"type":"block_actions",
            "team":{"id":"T7"},"user":{"id":"U2","team_id":"TFOREIGN"},"channel":{"id":"C1"},
            "message":{"ts":"12.5"},"actions":[{"action_id":"bot_approve","value":"nonce-1"}]}}"#;
        let (frame, ack) = parse_socket(raw);
        assert_eq!(ack.as_deref(), Some("E2"));
        assert_eq!(
            frame,
            Frame::Button(Button {
                team: "T7".into(),
                channel: "C1".into(),
                user: "U2".into(),
                message_ts: "12.5".into(),
                action_id: "bot_approve".into(),
                value: "nonce-1".into(),
            })
        );
        // 사용자 소속만 있고 설치 워크스페이스가 없으면 모호해서 버린다. 컨테이너의 채널·글 ID는 대신 쓴다
        let only_user = r#"{"type":"interactive","payload":{"type":"block_actions","user":{"id":"U2","team_id":"T1"},
            "channel":{"id":"C1"},"actions":[{"action_id":"bot_approve","value":"n"}]}}"#;
        assert_eq!(parse_socket(only_user).0, Frame::Ignored);
        let container = r#"{"type":"interactive","payload":{"type":"block_actions","team":{"id":"T7"},"user":{"id":"U2"},
            "container":{"channel_id":"C9","message_ts":"3.3"},"actions":[{"action_id":"bot_deny","value":"n"}]}}"#;
        match parse_socket(container).0 {
            Frame::Button(b) => {
                assert_eq!((b.channel.as_str(), b.message_ts.as_str()), ("C9", "3.3"))
            }
            other => panic!("{other:?}"),
        }
        // 버튼이 아닌 인터랙션
        let other = r#"{"type":"interactive","payload":{"type":"view_submission"}}"#;
        assert_eq!(parse_socket(other).0, Frame::Ignored);
    }

    #[test]
    fn reconnect_backoff_doubles_up_to_thirty_seconds() {
        assert_eq!(reconnect_delay_ms(0), 1000);
        assert_eq!(reconnect_delay_ms(3), 4000);
        for attempt in [30, 55, 64, 1_000, u32::MAX] {
            assert_eq!(reconnect_delay_ms(attempt), 30_000, "{attempt}");
        }
    }
}
