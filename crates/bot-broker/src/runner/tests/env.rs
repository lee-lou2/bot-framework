//! 도구가 고른 비밀 아닌 환경 변수: 선언한 이름·값만, 예약 이름과 비밀 이름은 거절, 호스트 환경은 쓰지 않는다.

use super::*;
use std::collections::BTreeSet;

fn with_env(names: &[&str], secrets: &[(&str, &str)]) -> Tool {
    let mut tool = decl(secrets, &[], false);
    tool.env = names.iter().map(|n| n.to_string()).collect::<BTreeSet<_>>();
    tool
}

fn job_with(tool: Tool, given: &[(&str, &str)]) -> ToolJob {
    let mut job = job(tool, r#"{"event":"x"}"#);
    job.env = given
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
    job
}

fn spec_env(r: &Rig) -> Vec<(String, String)> {
    r.sandbox.specs().remove(0).env
}

#[tokio::test]
async fn the_declared_values_reach_the_tool_and_nothing_else_does() {
    let r = plain();
    let ran = run(
        &r,
        job_with(
            with_env(&["CALENDAR_NAME", "TZ_LABEL"], &[("TOKEN", "reader")]),
            &[("CALENDAR_NAME", "personal"), ("TZ_LABEL", "Asia/Seoul")],
        ),
    )
    .await;
    assert!(matches!(ran, Ran::Done(_)), "{ran:?}");
    let env = spec_env(&r);
    // 비밀과 고른 설정만(선언한 프록시 없음 = 네트워크 없음). 호스트 환경에서 가져온 것은 하나도 없다.
    let names: BTreeSet<&str> = env.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        BTreeSet::from(["CALENDAR_NAME", "TOKEN", "TZ_LABEL"])
    );
    assert!(env.contains(&("CALENDAR_NAME".into(), "personal".into())));
    assert!(env.contains(&("TZ_LABEL".into(), "Asia/Seoul".into())));
}

#[tokio::test]
async fn a_tool_without_a_declared_env_gets_none_even_if_the_host_hands_over_values() {
    let r = plain();
    // 선언이 없는데 호스트가 값을 넘기면(어긋남) 시작하지 않는다
    let ran = run(
        &r,
        job_with(with_env(&[], &[]), &[("CALENDAR_NAME", "personal")]),
    )
    .await;
    assert!(matches!(ran, Ran::NotSent), "{ran:?}");
    assert!(r.sandbox.specs().is_empty());
    // 선언은 있는데 값이 없다 / 선언한 이름이 아닌 값 / 같은 이름이 둘
    for given in [
        vec![],
        vec![("OTHER", "x")],
        vec![("CALENDAR_NAME", "a"), ("CALENDAR_NAME", "b")],
        vec![("CALENDAR_NAME", "a"), ("OTHER", "x")],
    ] {
        let ran = run(&r, job_with(with_env(&["CALENDAR_NAME"], &[]), &given)).await;
        assert!(matches!(ran, Ran::NotSent), "{given:?}: {ran:?}");
    }
    assert!(r.sandbox.specs().is_empty());
}

#[tokio::test]
async fn names_that_steer_the_loader_the_shell_the_proxy_or_the_host_are_never_passed() {
    let r = plain();
    for name in [
        "PATH",
        "LD_PRELOAD",
        "NODE_OPTIONS",
        "HTTPS_PROXY",
        "NO_PROXY",
        "BOT_STATE_DIR",
        "BOT_EFFECT_URL",
        "CLAUDE_CODE_TMPDIR",
        "lowercase",
        "",
        "HAS SPACE",
    ] {
        let ran = run(&r, job_with(with_env(&[name], &[]), &[(name, "x")])).await;
        assert!(matches!(ran, Ran::NotSent), "{name:?}: {ran:?}");
    }
    assert!(r.sandbox.specs().is_empty(), "도구를 시작하지도 않는다");
}

#[tokio::test]
async fn an_env_name_cannot_shadow_a_declared_secret_and_values_are_bounded() {
    let r = plain();
    let ran = run(
        &r,
        job_with(
            with_env(&["TOKEN"], &[("TOKEN", "reader")]),
            &[("TOKEN", "from-config")],
        ),
    )
    .await;
    assert!(matches!(ran, Ran::NotSent), "{ran:?}");
    for value in ["has\0nul".to_owned(), "x".repeat(9 << 10)] {
        let ran = run(
            &r,
            job_with(with_env(&["LABEL"], &[]), &[("LABEL", &value)]),
        )
        .await;
        assert!(matches!(ran, Ran::NotSent), "{}", value.len());
    }
    assert!(r.sandbox.specs().is_empty());
}
