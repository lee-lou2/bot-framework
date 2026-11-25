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

fn post(frame: Frame) -> Post {
    match frame {
        Frame::Post(p) => p,
        other => panic!("{other:?}"),
    }
}

#[test]
fn channel_messages_become_posts_with_the_fields_the_handler_needs() {
    // 공개 채널(`message.channels`)의 스레드 답글: 사람 글
    let reply = events(
        r#"{"type":"message","channel_type":"channel","user":"U1","text":"답","ts":"20.2","thread_ts":"10.1","channel":"C1"}"#,
    );
    let p = post(parse_socket(&reply).0);
    assert_eq!(
        (
            p.team.as_str(),
            p.channel.as_str(),
            p.user.as_deref(),
            p.human
        ),
        ("T1", "C1", Some("U1"), true)
    );
    assert_eq!(
        (p.ts.as_str(), p.thread_ts.as_deref()),
        ("20.2", Some("10.1"))
    );
    assert_eq!(p.event_id.as_deref(), Some("Ev1"));
    // 비공개 채널(`message.groups`)·파일 글·스레드 방송 글도 사람의 글이다
    for ok in [
        r#"{"type":"message","channel_type":"group","user":"U1","text":"x","ts":"1.1","channel":"G1"}"#,
        r#"{"type":"message","subtype":"file_share","channel_type":"channel","user":"U1","ts":"1.1","channel":"C1","files":[{"id":"F1"}]}"#,
        r#"{"type":"message","subtype":"thread_broadcast","channel_type":"channel","user":"U1","text":"x","ts":"2.1","thread_ts":"1.1","channel":"C1"}"#,
    ] {
        assert!(post(parse_socket(&events(ok)).0).human, "{ok}");
    }
    // 봇·앱의 글은 담지만 사람의 글이 아니다(자기 봇의 글을 증거로 읽으려면 필요하다)
    for app in [
        r#"{"type":"message","channel_type":"channel","user":"UBOT","bot_id":"B1","text":"x","ts":"1.1","channel":"C1"}"#,
        r#"{"type":"message","subtype":"bot_message","channel_type":"channel","bot_id":"B2","text":"x","ts":"1.1","channel":"C1"}"#,
    ] {
        assert!(!post(parse_socket(&events(app)).0).human, "{app}");
    }
    // 수정·삭제·참여 알림, 보낸 곳을 모르는 글, 잘못된 ts, 다른 모양의 채널은 버린다
    for ignored in [
        r#"{"type":"message","subtype":"message_changed","channel_type":"channel","user":"U1","ts":"1.1","channel":"C1"}"#,
        r#"{"type":"message","subtype":"message_deleted","channel_type":"channel","ts":"1.1","channel":"C1"}"#,
        r#"{"type":"message","subtype":"channel_join","channel_type":"channel","user":"U1","ts":"1.1","channel":"C1"}"#,
        r#"{"type":"message","channel_type":"channel","user":"U1","text":"x","ts":"not-a-ts","channel":"C1"}"#,
        r#"{"type":"message","channel_type":"channel","user":"U1","text":"x","ts":"1.1","thread_ts":"../etc","channel":"C1"}"#,
        r#"{"type":"message","channel_type":"channel","user":"U1","text":"x","channel":"C1"}"#,
        r#"{"type":"message","channel_type":"mpim","user":"U1","text":"x","ts":"1.1","channel":"G1"}"#,
    ] {
        assert_eq!(
            parse_socket(&events(ignored)).0,
            Frame::Ignored,
            "{ignored}"
        );
    }
}

/// `bot_id` 없이 `app_id`만 있는 글도 앱 글이다: 사람 글로 분류해 스레드 답글 트리거를 일으키지 않는다.
#[test]
fn an_app_id_alone_marks_an_app_post_and_never_a_human_one() {
    let channel = events(
        r#"{"type":"message","channel_type":"channel","user":"U7","app_id":"A9","text":"x","ts":"1.1","thread_ts":"0.5","channel":"C1"}"#,
    );
    assert!(!post(parse_socket(&channel).0).human);
    // 같은 글이 스레드 밖 최상위 글이어도, 비공개 채널이어도 같다
    for raw in [
        r#"{"type":"message","channel_type":"group","user":"U7","app_id":"A9","text":"x","ts":"1.1","channel":"G1"}"#,
        r#"{"type":"message","subtype":"file_share","channel_type":"channel","user":"U7","app_id":"A9","text":"x","ts":"1.1","channel":"C1","files":[{"id":"F1"}]}"#,
    ] {
        assert!(!post(parse_socket(&events(raw)).0).human, "{raw}");
    }
    // DM·멘션 경로도 앱 글은 받지 않는다
    for raw in [
        r#"{"type":"message","channel_type":"im","user":"U7","app_id":"A9","text":"x","ts":"1.2","channel":"D1"}"#,
        r#"{"type":"app_mention","user":"U7","app_id":"A9","text":"<@UBOT> x","ts":"1.3","channel":"C1"}"#,
    ] {
        assert_eq!(parse_socket(&events(raw)).0, Frame::Ignored, "{raw}");
    }
    // 대조: 앱 표지가 없으면 사람 글이다
    let human = events(
        r#"{"type":"message","channel_type":"channel","user":"U7","text":"x","ts":"1.1","thread_ts":"0.5","channel":"C1"}"#,
    );
    assert!(post(parse_socket(&human).0).human);
}

/// 멘션·DM의 `ts`·`thread_ts`도 채널 글처럼 모양을 검사한다: 틀린 값은 요청 ID·대화 주소가 되기 전에 버린다.
#[test]
fn mentions_and_dms_check_their_timestamps_like_channel_posts() {
    let mention = |ts: &str, thread: Option<&str>| {
        let thread = thread.map_or(String::new(), |t| format!(r#","thread_ts":"{t}""#));
        events(&format!(
            r#"{{"type":"app_mention","user":"U1","text":"<@UBOT> x","ts":"{ts}"{thread},"channel":"C1"}}"#
        ))
    };
    let dm = |ts: &str, thread: Option<&str>| {
        let thread = thread.map_or(String::new(), |t| format!(r#","thread_ts":"{t}""#));
        events(&format!(
            r#"{{"type":"message","channel_type":"im","user":"U1","text":"x","ts":"{ts}"{thread},"channel":"D1"}}"#
        ))
    };
    for build in [&mention as &dyn Fn(&str, Option<&str>) -> String, &dm] {
        // 올바른 값은 받는다
        assert!(matches!(
            parse_socket(&build("20.2", Some("10.1"))).0,
            Frame::Message(_)
        ));
        assert!(matches!(
            parse_socket(&build("20.2", None)).0,
            Frame::Message(_)
        ));
        // 글 ts가 틀렸다
        for bad in [
            "not-a-ts",
            "../etc",
            "1.",
            ".1",
            "1",
            "1.2.3",
            "1.1 ",
            "9999999999999.1",
        ] {
            assert_eq!(
                parse_socket(&build(bad, None)).0,
                Frame::Ignored,
                "ts {bad}"
            );
            // 스레드 ts가 틀렸다
            assert_eq!(
                parse_socket(&build("20.2", Some(bad))).0,
                Frame::Ignored,
                "thread_ts {bad}"
            );
        }
    }
}

/// 문자열이 아닌 `thread_ts`(숫자·null·객체·빈 글)는 «없음»으로 보고 검사를 건너뛰지 않고 받지 않는다: 멘션·DM·채널 글이 모두 같다.
#[test]
fn a_thread_ts_that_is_not_a_string_is_refused_instead_of_being_skipped() {
    let kinds = [
        (
            r#""type":"app_mention","user":"U1","text":"<@UBOT> x","channel":"C1""#,
            "멘션",
        ),
        (
            r#""type":"message","channel_type":"im","user":"U1","text":"x","channel":"D1""#,
            "DM",
        ),
        (
            r#""type":"message","channel_type":"channel","user":"U1","text":"x","channel":"C1""#,
            "채널 글",
        ),
    ];
    for (head, name) in kinds {
        let with = |thread: &str| events(&format!(r#"{{{head},"ts":"20.2"{thread}}}"#));
        // 대조: 생략이거나 올바른 문자열이면 받는다
        assert!(
            !matches!(parse_socket(&with("")).0, Frame::Ignored),
            "{name}: 생략"
        );
        assert!(
            !matches!(
                parse_socket(&with(r#","thread_ts":"10.1""#)).0,
                Frame::Ignored
            ),
            "{name}: 올바른 문자열"
        );
        for bad in [
            r#","thread_ts":10.1"#,
            r#","thread_ts":10"#,
            r#","thread_ts":null"#,
            r#","thread_ts":{"a":1}"#,
            r#","thread_ts":["10.1"]"#,
            r#","thread_ts":true"#,
            r#","thread_ts":"""#,
        ] {
            assert_eq!(parse_socket(&with(bad)).0, Frame::Ignored, "{name}: {bad}");
        }
    }
}

/// 반응 이름은 Slack의 이모지 이름 글자(소문자·숫자·`_`·`+`·`-`·`:`)만 받는다: 이름은 트리거 조건과 기본 에이전트 입력에 그대로 실린다.
#[test]
fn reaction_names_follow_the_emoji_alphabet_and_nothing_else() {
    let react = |name: &str| {
        events(&format!(
            r#"{{"type":"reaction_added","user":"U2","reaction":"{name}","item":{{"type":"message","channel":"C1","ts":"30.3"}}}}"#
        ))
    };
    for ok in ["white_check_mark", "+1", "-1", "thumbsup::skin-tone-2", "a"] {
        assert!(
            matches!(parse_socket(&react(ok)).0, Frame::Reaction(_)),
            "{ok}"
        );
    }
    for bad in [
        "",
        "Upper",
        "two words",
        "tab\\t",
        "quote\\\"x",
        "brace{x}",
        "a,b",
        "a/b",
        "한글",
        "x;y",
        "<x>",
    ] {
        assert_eq!(parse_socket(&react(bad)).0, Frame::Ignored, "{bad:?}");
    }
    assert_eq!(
        parse_socket(&react(&"a".repeat(101))).0,
        Frame::Ignored,
        "100자를 넘는 이름"
    );
}

#[test]
fn a_reaction_on_a_message_keeps_the_actor_and_the_target() {
    let raw = events(
        r#"{"type":"reaction_added","user":"U2","reaction":"white_check_mark","item_user":"U9","item":{"type":"message","channel":"C1","ts":"30.3"},"event_ts":"31.1"}"#,
    );
    match parse_socket(&raw).0 {
        Frame::Reaction(r) => assert_eq!(
            (
                r.team.as_str(),
                r.channel.as_str(),
                r.item_ts.as_str(),
                r.user.as_str(),
                r.reaction.as_str(),
                r.item_user.as_deref(),
                r.event_id.as_str(),
            ),
            (
                "T1",
                "C1",
                "30.3",
                "U2",
                "white_check_mark",
                Some("U9"),
                "Ev1"
            )
        ),
        other => panic!("{other:?}"),
    }
    // 글이 아닌 대상(파일)·빠진 값·잘못된 ts·모양이 이상한 이름은 버린다
    for ignored in [
        r#"{"type":"reaction_added","user":"U2","reaction":"x","item":{"type":"file","file":"F1"}}"#,
        r#"{"type":"reaction_added","reaction":"x","item":{"type":"message","channel":"C1","ts":"30.3"}}"#,
        r#"{"type":"reaction_added","user":"U2","item":{"type":"message","channel":"C1","ts":"30.3"}}"#,
        r#"{"type":"reaction_added","user":"U2","reaction":"x","item":{"type":"message","channel":"C1","ts":"bad"}}"#,
        r#"{"type":"reaction_added","user":"U2","reaction":"two words","item":{"type":"message","channel":"C1","ts":"30.3"}}"#,
    ] {
        assert_eq!(
            parse_socket(&events(ignored)).0,
            Frame::Ignored,
            "{ignored}"
        );
    }
    // 이벤트 ID가 없으면 재전달을 가를 수 없어 받지 않는다
    let no_id = r#"{"type":"events_api","envelope_id":"E9","payload":{"team_id":"T1","event":{"type":"reaction_added","user":"U2","reaction":"x","item":{"type":"message","channel":"C1","ts":"30.3"}}}}"#;
    assert_eq!(parse_socket(no_id).0, Frame::Ignored);
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
