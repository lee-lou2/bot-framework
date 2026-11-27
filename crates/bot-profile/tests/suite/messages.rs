//! «처음 쓰는 사람» 오류 문구(M1~M7): 틀린 곳을 알려 주는 데서 끝나지 않고 **어느 파일의 어느 필드를 무엇으로 고칠지**(허용 값·비슷한 이름·예시·호스트 설정 위치)를 말한다.
//! 정규식 원문·파서의 영어 설명은 사용자에게 나가지 않는다. 문구가 바뀌면 이 시험이 알려 준다.

use crate::support::{TOOLS_PATH, Temp, base, check, edit, host, write_profile};
use bot_profile::{Issue, Limits, load};

/// 오류 한 건씩(코드·경로·위치·문구)을 돌려준다. 통과하면 시험이 틀린 것이다.
fn errors(files: &crate::support::Files) -> Vec<Issue> {
    match check(files) {
        Ok(_) => panic!("거절해야 하는데 통과했어요"),
        Err(report) => report.errors().cloned().collect(),
    }
}

fn only(files: &crate::support::Files) -> Issue {
    let mut found = errors(files);
    assert_eq!(found.len(), 1, "{found:#?}");
    found.remove(0)
}

fn hangul(text: &str) -> bool {
    text.chars().any(|c| ('가'..='힣').contains(&c))
}

#[test]
fn m1_a_wrong_adapter_lists_every_allowed_value_and_the_closest_one() {
    let issue = only(&edit(base(), "profile.yaml", "adapter: cli", "adapter: cl"));
    assert_eq!(
        (issue.code, issue.path.as_str(), issue.at.as_str()),
        ("schema", "personal/profile.yaml", "/bindings/0/adapter")
    );
    assert!(
        issue.message.contains(r#""cli", "slack", "http""#),
        "{}",
        issue.message
    );
    assert!(issue.message.contains(r#"받은 값: "cl""#) && issue.message.contains(r#"혹시 "cli""#));
    // 한 변형만 있는 것처럼 읽히던 예전 문구는 없다
    assert!(!issue.message.contains(r#"값은 "cli"이어야"#));
    // 값이 맞는 변형이면 그 변형의 모자란 필드를 알려 준다
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "workspace: home, channel: planning,",
        "workspace: home,",
    ));
    assert_eq!(issue.at, "/bindings/2");
    assert!(
        issue.message.contains("필수 필드 `channel`"),
        "{}",
        issue.message
    );
}

#[test]
fn m2_a_model_alias_the_host_does_not_allow_names_the_allowed_ones_and_where_to_add() {
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "model: assistant",
        "model: assistent",
    ));
    assert_eq!(
        (issue.code, issue.path.as_str(), issue.at.as_str()),
        ("model-not-allowed", "personal/profile.yaml", "/model")
    );
    let text = &issue.message;
    assert!(
        text.contains("`assistent`") && text.contains("프로필 `personal`"),
        "{text}"
    );
    assert!(text.contains("허용된 별칭: assistant, fast"), "{text}");
    assert!(text.contains("혹시 `assistant`인가요?"), "{text}");
    assert!(
        text.contains("호스트 설정(BOT_HOST_CONFIG)의 `models.personal`"),
        "{text}"
    );
    // 에이전트가 자기 AGENT.md에서 고른 별칭은 그 파일을 가리킨다(부모에게서 물려받은 곳은 따로 세지 않는다)
    let issue = only(&edit(
        base(),
        "agents/main/agents/research/AGENT.md",
        "부모가",
        "---\nmodel: fastt\n---\n부모가",
    ));
    assert_eq!(
        (issue.code, issue.path.as_str()),
        (
            "model-not-allowed",
            "personal/agents/main/agents/research/AGENT.md"
        )
    );
    assert!(
        issue.message.contains("혹시 `fast`인가요?"),
        "{}",
        issue.message
    );
    // 이 프로필의 허용 목록이 비어 있으면 호스트 설정에 넣을 한 줄을 보여 준다
    let root = Temp::new("models");
    write_profile(root.path(), "personal", &base());
    let mut empty = host();
    empty.models.clear();
    let report = load(root.path(), &empty, &Limits::default()).unwrap_err();
    let issue = report.errors().next().unwrap();
    assert!(
        issue
            .message
            .contains(r#""models": {"personal": ["assistant"]}"#),
        "{}",
        issue.message
    );
}

#[test]
fn m3_name_format_errors_say_what_is_allowed_with_an_example_instead_of_a_regex() {
    // 도구 id(소문자 kebab-case)
    let issue = only(&edit(base(), TOOLS_PATH, "id: list", "id: List"));
    assert_eq!(issue.at, "/tools/0/id");
    assert!(
        issue
            .message
            .contains("소문자로 시작하고 소문자·숫자·하이픈")
            && issue.message.contains("예: `daily-report`"),
        "{}",
        issue.message
    );
    // 자격 변수 이름(대문자·숫자·밑줄)
    let issue = only(&edit(
        base(),
        TOOLS_PATH,
        "secrets: {CALENDAR_TOKEN: calendar-reader}",
        "secrets: {calendar_token: calendar-reader}",
    ));
    assert!(
        issue.message.contains("대문자·숫자·밑줄")
            && issue.message.contains("예: `CALENDAR_TOKEN`"),
        "{}",
        issue.message
    );
    // 에이전트 이름: 같은 규칙을 말하는 두 패턴이 한 줄로 합쳐진다
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "agent: main}\n  - {id: inbox",
        "agent: Main}\n  - {id: inbox",
    ));
    assert_eq!(issue.at, "/bindings/0/agent");
    assert!(
        issue.message.contains("예: `main/research`"),
        "{}",
        issue.message
    );
    // 어디에도 정규식 원문이 없다
    let text = &issue.message;
    assert!(
        !text.contains("(?:") && !text.contains("^[") && !text.contains("패턴"),
        "{text}"
    );
}

#[test]
fn m4_a_misspelled_field_is_one_line_naming_the_field_it_should_be() {
    let issue = only(&edit(base(), "profile.yaml", "bindings:", "bindngs:"));
    assert_eq!(issue.at, "/bindngs");
    assert!(
        issue.message.contains("`bindngs`") && issue.message.contains("`bindings`를 잘못 쓴"),
        "{}",
        issue.message
    );
    // 선택 필드의 오타도 한 줄이다
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "secrets: [calendar-reader]",
        "secrts: [calendar-reader]",
    ));
    assert!(
        issue.message.contains("`secrets`를 잘못 쓴"),
        "{}",
        issue.message
    );
    // 오타가 아닌 알 수 없는 필드는 쓸 수 있는 필드를 알려 준다
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "secrets: [calendar-reader]",
        "secrets: [calendar-reader]\nextra: 1",
    ));
    assert!(
        issue.message.contains("쓸 수 있는 필드:") && issue.message.contains("bindings"),
        "{}",
        issue.message
    );
}

#[test]
fn m5_an_entrance_used_twice_says_how_to_separate_the_two_bindings() {
    let twice = edit(
        base(),
        "profile.yaml",
        "  - {id: inbox, adapter: http",
        "  - {id: desk2, adapter: cli, account: owner, agent: main}\n  - {id: inbox, adapter: http",
    );
    let issue = only(&twice);
    assert_eq!(issue.code, "duplicate-entrance");
    assert!(
        issue.message.contains("바인딩 `desk`와 `desk2`")
            && issue.message.contains("`account`를 다른 별칭으로"),
        "{}",
        issue.message
    );
    // 다른 프로필이 같은 CLI 계정 별칭을 쓴다: 호스트 설정 위치와 프로필 고르기까지 알려 준다
    let root = Temp::new("m5");
    write_profile(root.path(), "personal", &base());
    let second = vec![
        (
            "profile.yaml".to_owned(),
            "version: 1\nid: second\nmodel: assistant\nroles: {owner: [run]}\nmembers: [{identity: \"local:owner\", roles: [owner]}]\nbindings:\n  - {id: desk, adapter: cli, account: owner, agent: main}\n".to_owned(),
        ),
        ("agents/main/AGENT.md".to_owned(), "두 번째.\n".to_owned()),
    ];
    write_profile(root.path(), "second", &second);
    let report = load(root.path(), &host(), &Limits::default()).unwrap_err();
    let issue = report.errors().next().unwrap();
    assert_eq!(issue.code, "duplicate-entrance");
    let text = &issue.message;
    assert!(text.contains("프로필 personal의 바인딩 desk"), "{text}");
    assert!(text.contains("`account`를 다른 별칭으로"), "{text}");
    assert!(text.contains("`gateway.cli.accounts`"), "{text}");
    assert!(text.contains("bot chat --profile"), "{text}");
    // Slack 채널·DM·HTTP도 자기 필드를 말한다
    for (key, field) in [
        ("slack:home:channel:planning", "`channel`"),
        ("slack:home:dm", "`workspace`"),
        ("http:reminders", "`endpoint`"),
    ] {
        let fix = bot_profile_hint_for(key);
        assert!(fix.contains(field), "{key}: {fix}");
    }
}

/// 입구 종류별 고치는 방법(공개 API가 아니라 두 프로필이 겹친 실제 사례로 확인한다).
fn bot_profile_hint_for(key: &str) -> String {
    let (adapter, edit_to) = match key {
        "slack:home:channel:planning" => (
            "  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main}",
            "  - {id: team2, adapter: slack, workspace: home, channel: planning, agent: main}\n  - {id: team, adapter: slack, workspace: home, channel: planning, agent: main}",
        ),
        "slack:home:dm" => (
            "  - {id: direct, adapter: slack, workspace: home, dm: true, agent: main}",
            "  - {id: direct2, adapter: slack, workspace: home, dm: true, agent: main}\n  - {id: direct, adapter: slack, workspace: home, dm: true, agent: main}",
        ),
        _ => (
            "  - {id: inbox, adapter: http, endpoint: reminders, agent: main}",
            "  - {id: inbox2, adapter: http, endpoint: reminders, agent: main}\n  - {id: inbox, adapter: http, endpoint: reminders, agent: main}",
        ),
    };
    only(&edit(base(), "profile.yaml", adapter, edit_to)).message
}

#[test]
fn m7_yaml_syntax_errors_say_how_to_fix_it_in_korean_with_line_and_column() {
    let cases = [
        (
            "model: assistant: x",
            "앞 줄과 들여쓰기가 맞는지",
            "한 줄에 `:`가 두 번",
        ),
        ("model:\tassistant", "`:` 뒤에는 공백 한 칸", "탭"),
        (
            "secrets: [calendar-reader",
            "닫지 않은 `[`",
            "따옴표로 감싸세요",
        ),
    ];
    for (broken, first, second) in cases {
        let from = if broken.starts_with("secrets") {
            "secrets: [calendar-reader]"
        } else {
            "model: assistant"
        };
        let issue = only(&edit(base(), "profile.yaml", from, broken));
        assert_eq!(issue.code, "yaml-syntax", "{broken}");
        assert!(
            issue.at.starts_with("줄 ") && issue.at.contains(", 칸 "),
            "{broken}: {}",
            issue.at
        );
        assert!(hangul(&issue.message), "{broken}: {}", issue.message);
        assert!(
            issue.message.contains(first) || issue.message.contains(second),
            "{broken}: {}",
            issue.message
        );
        // 파서의 영어 설명은 사용자에게 나가지 않는다
        for english in [
            "not allowed in this context",
            "must be followed by",
            "did not find",
            "illegal placement",
        ] {
            assert!(
                !issue.message.contains(english),
                "{broken}: {}",
                issue.message
            );
        }
    }
    // 들여쓰기가 어긋난 줄
    let issue = only(&edit(
        base(),
        "profile.yaml",
        "roles:\n  owner:",
        "roles:\n   owner:\n  bad",
    ));
    assert!(issue.message.contains("들여쓰기"), "{}", issue.message);
}
