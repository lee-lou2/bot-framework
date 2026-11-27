use super::*;
use crate::trace::SpanIds;
use bot_core::id::{Attempt, Generation, ProfileId, RunId};

// 토큰 모양 리터럴이 소스에 남지 않게 접두사를 실행 중에 이어 붙인다.
fn token() -> String {
    format!("{}-{}", "xoxb", "1234567890-abcdefghij")
}

fn span(attrs: Vec<(Key, Val)>) -> SpanRecord {
    let a = Attempt {
        profile: ProfileId("alpha".into()),
        run: RunId("r-1".into()),
        claim: None,
        generation: Generation(1),
    };
    let mut s = SpanRecord::start(&a.profile, "execute_tool agenda.list", SpanIds::attempt(&a));
    s.attrs = attrs;
    s
}

fn get<'a>(o: &'a Out, k: &str) -> Option<&'a Val> {
    o.attrs.iter().find(|(x, _)| *x == k).map(|(_, v)| v)
}

const OFF: Policy = Policy {
    ids: Ids::Plain,
    content: false,
    private: false,
};

#[test]
fn ordinary_text_survives_and_secrets_of_any_length_are_masked() {
    let r = Redactor::new(["zq7".to_owned(), "alpha-token-1".to_owned(), String::new()]);
    let plain = "일반 문구입니다. INPUT-MARK-1 그대로 남아야 한다.";
    assert_eq!(r.mask(plain), plain);
    let out = r.mask("답: ANSWER-MARK-2, 키는 zq7 그리고 alpha-token-1 입니다 zq7zq7");
    assert!(out.contains("ANSWER-MARK-2") && out.starts_with("답: "));
    assert!(
        !out.contains("zq7") && !out.contains("alpha-token-1"),
        "{out}"
    );
    assert_eq!(out.matches(MASK).count(), 4, "{out}");
    assert!(!r.leaks(&out), "가린 글은 누출이 아니다");
}

#[test]
fn token_shapes_are_masked_and_the_rest_is_kept() {
    let r = Redactor::default();
    let out = r.mask(&format!("ERROR-MARK-3 실패 {} 끝", token()));
    assert!(
        out.contains("ERROR-MARK-3 실패 ") && out.ends_with(" 끝") && !out.contains(&token()),
        "{out}"
    );
    assert!(r.leaks(&format!("token {}", token())));
    let refresh = format!("{}_{}", "ghr", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6");
    assert!(r.leaks(&refresh) && r.leaks_meta(&refresh));
    assert_eq!(r.mask(&format!("tok {refresh} end")), "tok [redacted] end");
    let jwt = format!(
        "{}.{}.{}",
        "eyJhbGciOiJIUzI1NiJ9", "eyJzdWIiOiJhYmMifQ", "c2lnbmF0dXJlLXZhbHVl"
    );
    assert!(r.leaks(&jwt) && !r.mask(&jwt).contains("eyJ"));
    assert!(r.leaks(&"a1".repeat(30)), "길고 무작위처럼 보이는 문자열");
    assert!(
        !r.leaks("agenda.list main/research r-17 c-9 012345"),
        "고정 어휘·짧은 ID는 통과"
    );
}

#[test]
fn long_hex_revisions_pass_as_metadata_but_are_masked_in_free_text() {
    let r = Redactor::default();
    let revision = "0123456789abcdef".repeat(4);
    assert!(!r.leaks_meta(&revision) && r.leaks(&revision));
    assert!(!r.has_value(&revision));
    let o = apply(
        span(vec![(key::BOT_TOOL_REVISION, revision.clone().into())]),
        &OFF,
        &r,
    );
    assert_eq!(
        get(&o, key::BOT_TOOL_REVISION),
        Some(&Val::Str(revision.clone())),
        "판본 해시는 메타데이터로 나간다"
    );
    let on = Policy {
        ids: Ids::Plain,
        content: true,
        private: false,
    };
    let o = apply(
        span(vec![(
            key::GEN_AI_INPUT_MESSAGES,
            format!("키 {revision} 끝").into(),
        )]),
        &on,
        &r,
    );
    assert!(
        matches!(get(&o, key::GEN_AI_INPUT_MESSAGES), Some(Val::Str(t)) if !t.contains(&revision) && t.ends_with(" 끝"))
    );
    // 알려진 접두사 모양은 메타데이터에서도 막는다.
    assert!(r.leaks_meta(&format!("{}-{}", "xoxb", "1234567890-abcdefghij")));
    let known = Redactor::new(["zq7".to_owned()]);
    assert!(known.has_value("a zq7 b") && !known.has_value("a zq b") && known.leaks_meta("zq7"));
}

#[test]
fn a_private_span_never_exports_content_even_when_the_profile_enabled_it() {
    let on = Policy {
        ids: Ids::Plain,
        content: true,
        private: false,
    };
    let red = Redactor::default();
    let rec = |private: bool| {
        let mut attrs = vec![
            (
                key::GEN_AI_INPUT_MESSAGES,
                Val::Str("개인 대화 원문".into()),
            ),
            (key::GEN_AI_TOOL_NAME, Val::Str("inbox.receive".into())),
        ];
        if private {
            attrs.push((PRIVATE, Val::Bool(true)));
        }
        span(attrs)
    };
    let o = apply(rec(true), &on, &red);
    assert!(get(&o, key::GEN_AI_INPUT_MESSAGES).is_none());
    assert_eq!(
        get(&o, key::BOT_CONTENT_DROPPED),
        Some(&Val::Str("private".into()))
    );
    assert_eq!(
        get(&o, key::GEN_AI_TOOL_NAME),
        Some(&Val::Str("inbox.receive".into()))
    );
    assert!(get(&o, PRIVATE).is_none(), "내부 표지는 내보내지 않는다");
    assert_eq!(
        get(&o, key::BOT_DROPPED),
        Some(&Val::Int(1)),
        "표지는 버린 것으로 세지 않는다"
    );
    // 표지가 없는 같은 프로필의 스팬은 내용이 나간다.
    let o = apply(rec(false), &on, &red);
    assert!(get(&o, key::GEN_AI_INPUT_MESSAGES).is_some());
    // 표지가 false이면 일반 스팬이다.
    let mut plain = rec(false);
    plain.attrs.push((PRIVATE, Val::Bool(false)));
    assert!(get(&apply(plain, &on, &red), key::GEN_AI_INPUT_MESSAGES).is_some());
}

#[test]
fn overlapping_values_mask_the_longer_one_first() {
    let r = Redactor::new(["abc".to_owned(), "abcdef-long".to_owned()]);
    assert_eq!(
        r.mask("x abcdef-long y abc z"),
        format!("x {MASK} y {MASK} z")
    );
}

#[test]
fn content_is_dropped_by_default_and_marked() {
    let rec = span(vec![
        (key::GEN_AI_INPUT_MESSAGES, "원문 입력".into()),
        (key::GEN_AI_TOOL_CALL_ARGUMENTS, "{\"event\":\"x\"}".into()),
        (key::GEN_AI_TOOL_NAME, "agenda.list".into()),
    ]);
    let o = apply(rec, &OFF, &Redactor::default());
    assert!(
        get(&o, key::GEN_AI_INPUT_MESSAGES).is_none()
            && get(&o, key::GEN_AI_TOOL_CALL_ARGUMENTS).is_none()
    );
    assert_eq!(
        get(&o, key::GEN_AI_TOOL_NAME),
        Some(&Val::Str("agenda.list".into()))
    );
    assert_eq!(
        get(&o, key::BOT_CONTENT_DROPPED),
        Some(&Val::Str("off".into()))
    );
    assert_eq!(get(&o, key::BOT_DROPPED), Some(&Val::Int(2)));
}

#[test]
fn enabled_content_is_masked_and_secret_leftovers_drop_the_attribute() {
    let on = Policy {
        ids: Ids::Plain,
        content: true,
        private: false,
    };
    let r = Redactor::new(["zq7".to_owned()]);
    let rec = span(vec![(
        key::GEN_AI_OUTPUT_MESSAGES,
        format!("답 zq7 {}", token()).into(),
    )]);
    let o = apply(rec, &on, &r);
    let Some(Val::Str(text)) = get(&o, key::GEN_AI_OUTPUT_MESSAGES) else {
        panic!("내용이 빠졌다")
    };
    assert!(
        !text.contains("zq7") && !text.contains(&token()) && text.starts_with("답 "),
        "{text}"
    );
    assert!(get(&o, key::BOT_CONTENT_DROPPED).is_none());
    let long = "가".repeat(20_000);
    let o = apply(
        span(vec![(key::GEN_AI_INPUT_MESSAGES, long.into())]),
        &on,
        &r,
    );
    assert!(
        matches!(get(&o, key::GEN_AI_INPUT_MESSAGES), Some(Val::Str(s)) if s.chars().count() == CONTENT_LIMIT)
    );
}

#[test]
fn unknown_keys_and_secret_valued_attributes_never_leave() {
    let r = Redactor::new(["zq7".to_owned()]);
    let rec = span(vec![
        ("bot.approval.nonce", "ap-secret-nonce".into()),
        (key::BOT_AGENT, "has zq7 inside".into()),
        (key::BOT_TRIGGER, "morning".into()),
        (key::BOT_PROFILE, "other-profile".into()),
    ]);
    let o = apply(rec, &OFF, &r);
    assert!(get(&o, "bot.approval.nonce").is_none() && get(&o, key::BOT_AGENT).is_none());
    assert_eq!(get(&o, key::BOT_TRIGGER), Some(&Val::Str("morning".into())));
    assert_eq!(
        get(&o, key::BOT_PROFILE),
        Some(&Val::Str("alpha".into())),
        "프로필은 레코드 것으로 고정"
    );
    assert_eq!(get(&o, key::BOT_DROPPED), Some(&Val::Int(2)));
    assert_eq!(
        get(&o, key::BOT_CONTENT_DROPPED),
        Some(&Val::Str("secret".into()))
    );
    assert_eq!(
        o.attrs
            .iter()
            .filter(|(k, _)| *k == key::BOT_PROFILE)
            .count(),
        1
    );
}

#[test]
fn identity_keys_become_stable_per_profile_pseudonyms_when_hashed() {
    let hash = Policy {
        ids: Ids::Hash,
        content: false,
        private: false,
    };
    let red = Redactor::default();
    let a = apply(
        span(vec![
            (key::BOT_PRINCIPAL, "local:owner".into()),
            (key::BOT_AGENT, "main".into()),
        ]),
        &hash,
        &red,
    );
    let b = apply(
        span(vec![(key::BOT_PRINCIPAL, "local:owner".into())]),
        &hash,
        &red,
    );
    let Some(Val::Str(x)) = get(&a, key::BOT_PRINCIPAL) else {
        panic!("가명 없음")
    };
    assert_eq!(x.len(), 12);
    assert!(!x.contains("owner"));
    assert_eq!(
        Some(&Val::Str(x.clone())),
        get(&b, key::BOT_PRINCIPAL),
        "같은 값은 같은 가명"
    );
    assert_eq!(
        get(&a, key::BOT_AGENT),
        Some(&Val::Str("main".into())),
        "식별자가 아닌 키는 그대로"
    );
    assert_ne!(pseudonym("alpha", "v"), pseudonym("beta", "v"));
    let plain = apply(
        span(vec![(key::BOT_PRINCIPAL, "local:owner".into())]),
        &OFF,
        &red,
    );
    assert_eq!(
        get(&plain, key::BOT_PRINCIPAL),
        Some(&Val::Str("local:owner".into()))
    );
}

#[test]
fn names_events_and_counts_are_bounded() {
    let r = Redactor::new(["zq7".to_owned()]);
    let mut rec = span(Vec::new());
    rec.name = "chat zq7".into();
    for i in 0..40u64 {
        rec = rec.event(
            key::BOT_CONTENT_DROPPED,
            vec![(key::BOT_OUTCOME, Val::Int(i64::try_from(i).unwrap_or(0)))],
        );
    }
    rec.links = (0..12u8)
        .map(|i| crate::trace::LinkRecord {
            trace: [i + 1; 16],
            span: [i + 1; 8],
        })
        .collect();
    rec.attrs = (0..100i64)
        .map(|i| (key::BOT_OUTCOME, Val::Int(i)))
        .collect();
    let o = apply(rec, &OFF, &r);
    assert_eq!(o.name, MASK);
    assert_eq!((o.events.len(), o.links.len()), (MAX_EVENTS, MAX_LINKS));
    assert_eq!(
        o.events[0].0, "bot.content.dropped",
        "이벤트 이름은 고정 어휘로 통과"
    );
    assert_eq!(
        o.attrs
            .iter()
            .filter(|(k, _)| *k == key::BOT_OUTCOME)
            .count(),
        MAX_ATTRS
    );
    assert!(o.dropped >= 36 + 24 + 4, "{}", o.dropped);
}

#[test]
fn values_added_later_are_masked_by_every_clone_and_never_unlearned() {
    let red = Redactor::new(["first-secret-value".to_owned()]);
    let shared = red.clone();
    let text = "key second-secret-value and first-secret-value";
    assert!(!red.mask(text).contains("first-secret-value"));
    assert!(
        red.mask(text).contains("second-secret-value"),
        "아직 모르는 값은 그대로다"
    );
    // 키를 교체했다: 새 값을 한 복제본에 더하면 모든 복제본이 곧바로 가린다
    red.add("second-secret-value");
    for each in [&red, &shared] {
        assert!(each.has_value("second-secret-value") && each.leaks_meta("x second-secret-value"));
        assert!(
            !each.mask(text).contains("secret-value"),
            "{}",
            each.mask(text)
        );
    }
    // 같은 값·빈 값은 목록을 늘리지 않고, 값이 많아도 긴 것부터 가린다(겹치는 값에서 긴 쪽을 먼저)
    red.add("second-secret-value");
    red.add("");
    assert!(format!("{red:?}").contains("values: 2"), "{red:?}");
    red.add("secret");
    assert_eq!(
        red.mask("a second-secret-value b"),
        "a [redacted] b",
        "짧은 값 `secret`이 긴 값의 일부를 먼저 지워 조각이 남지 않는다"
    );
}

#[test]
fn execute_tool_standard_attributes_pass_allowed_vocabulary_policy() {
    let red = Redactor::default();
    let rec = || {
        span(vec![
            (key::GEN_AI_OPERATION_NAME, "execute_tool".into()),
            (key::GEN_AI_TOOL_TYPE, "function".into()),
            (key::GEN_AI_TOOL_NAME, "agenda.list".into()),
            (key::GEN_AI_TOOL_CALL_ID, "c-1".into()),
            (key::BOT_ATTEMPT_GENERATION, 1u64.into()),
            (key::BOT_CALL_STATE, "done".into()),
            (key::BOT_TOOL_RISK, "read".into()),
        ])
    };
    for policy in [
        OFF,
        Policy {
            ids: Ids::Hash,
            content: false,
            private: false,
        },
    ] {
        let o = apply(rec(), &policy, &red);
        assert_eq!(
            get(&o, key::GEN_AI_OPERATION_NAME),
            Some(&Val::Str("execute_tool".into()))
        );
        assert_eq!(
            get(&o, key::GEN_AI_TOOL_TYPE),
            Some(&Val::Str("function".into()))
        );
        assert_eq!(
            get(&o, key::GEN_AI_TOOL_NAME),
            Some(&Val::Str("agenda.list".into()))
        );
        assert_eq!(o.dropped, 0);
        assert!(get(&o, key::BOT_DROPPED).is_none());
    }
}
