//! tools.yaml `env`(도구에 전달할 일반 설정 이름): 상속을 마친 env.yaml에 이름이 있어야 하고, 예약 이름·비밀 환경 이름과 겹치면 안 되며,
//! 이름 목록은 도구 판본에 들어간다. 호스트 내장 도구 ID(기억 공유·예약)는 프로필이 선언할 수 없다.

use crate::support::{Files, TOOLS_PATH, agent, base, check, edit, ok, raw_bundle, snapshot};
use bot_core::{
    id::{ProfileId, ToolId},
    profile::{Snapshot, Tool},
};
use bot_profile::{Limits, from_bundle, revision_of};
use std::collections::BTreeSet;

const CHILD_ENV: &str = "agents/main/agents/research/env.yaml";

/// 기본 프로필의 `agenda.list`에 `env: <목록>`을 붙인다.
fn with_env(list: &str) -> Files {
    edit(
        base(),
        TOOLS_PATH,
        "egress: [\"calendar.example:443\"]\n",
        &format!("egress: [\"calendar.example:443\"]\n    env: {list}\n"),
    )
}

fn list_tool<'a>(snap: &'a Snapshot, agent_path: &str) -> &'a Tool {
    &snap.agents[&agent(agent_path)].skills["agenda"].tools[&ToolId("agenda.list".into())]
}

fn report_of(files: &Files) -> String {
    format!("{}", check(files).expect_err("거절해야 해요"))
}

fn names(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn chosen_names_reach_the_tool_and_its_revision_and_empty_changes_nothing() {
    let plain = snapshot(&base());
    let chosen = snapshot(&with_env("[AGENDA_VIEW]"));
    assert!(list_tool(&plain, "main").env.is_empty(), "기본은 빈 집합");
    assert_eq!(list_tool(&chosen, "main").env, names(&["AGENDA_VIEW"]));
    assert_eq!(
        list_tool(&chosen, "main/research").env,
        names(&["AGENDA_VIEW"]),
        "자식도 상속한 env로 같은 도구를 받는다"
    );
    assert_ne!(
        list_tool(&chosen, "main").revision,
        list_tool(&plain, "main").revision,
        "이름 목록은 도구 판본에 들어간다"
    );
    assert_eq!(
        list_tool(&snapshot(&with_env("[]")), "main").revision,
        list_tool(&plain, "main").revision,
        "빈 목록은 쓰지 않은 것과 같은 판본(옛 판본이 바뀌지 않는다)"
    );
    let delete = |s: &Snapshot| {
        s.agents[&agent("main")].skills["agenda"].tools[&ToolId("agenda.delete".into())]
            .revision
            .clone()
    };
    assert_eq!(delete(&chosen), delete(&plain), "다른 도구의 판본은 그대로");
}

#[test]
fn every_calling_agent_must_have_the_name_after_inheritance() {
    // 어느 에이전트에도 없음: 부모·자식 모두 호출할 수 있으니 둘 다 알린다(한 건으로 묶는다)
    let none = report_of(&with_env("[NOPE_KEY]"));
    assert!(none.contains("env-missing"), "{none}");
    assert!(none.contains("main · main/research"), "{none}");

    // 자식에만 있음: 부모(main)가 이 스킬을 부를 수 있으니 거절
    let child_only = edit(with_env("[ONLY_CHILD]"), CHILD_ENV, "", "ONLY_CHILD: x\n");
    let found = report_of(&child_only);
    assert!(found.contains("env-missing"), "{found}");
    assert!(
        found.contains("main의 env.yaml") && !found.contains("main/research의"),
        "부모만 빠졌다고 알려야 해요: {found}"
    );

    // 부모에 있으면 자식은 상속하므로 통과
    ok(&with_env("[AGENDA_VIEW]"));
    // 자식이 값을 덮어써도(이름은 그대로) 통과
    ok(&edit(
        with_env("[AGENDA_VIEW]"),
        CHILD_ENV,
        "",
        "AGENDA_VIEW: week\n",
    ));
}

#[test]
fn rejects_reserved_loader_shell_proxy_and_host_names() {
    let with_name = |name: &str| {
        edit(
            with_env(&format!("[{name}]")),
            "agents/main/env.yaml",
            "AGENDA_VIEW: today\n",
            &format!("AGENDA_VIEW: today\n{name}: x\n"),
        )
    };
    // env.yaml은 허용하지만 도구 환경으로는 못 고르는 이름
    for name in [
        "HOME",
        "TMPDIR",
        "NODE_OPTIONS",
        "GIT_DIR",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "SRT_DEBUG",
        "BASH_ENV",
    ] {
        let found = report_of(&with_name(name));
        assert!(found.contains("env-reserved"), "{name}\n{found}");
        assert!(!found.contains("env-missing"), "{name}\n{found}");
    }
    // env.yaml 단계에서 이미 거절되는 이름은 거기서 막힌다
    for name in ["PATH", "LD_PRELOAD", "BOT_TOKEN"] {
        let found = report_of(&with_name(name));
        assert!(found.contains("reserved-env"), "{name}\n{found}");
    }
}

#[test]
fn rejects_a_name_that_is_also_a_secret_environment_name() {
    let files = edit(
        with_env("[CALENDAR_TOKEN]"),
        "agents/main/env.yaml",
        "AGENDA_VIEW: today\n",
        "AGENDA_VIEW: today\nCALENDAR_TOKEN: plain\n",
    );
    let found = report_of(&files);
    assert!(found.contains("env-secret-conflict"), "{found}");
    assert!(
        !found.contains("env-missing"),
        "이름은 있으니 누락이 아니에요: {found}"
    );
}

#[test]
fn schema_rejects_duplicates_bad_names_and_wrong_shapes() {
    for list in [
        "[AGENDA_VIEW, AGENDA_VIEW]",
        "[agenda_view]",
        "[\"A-B\"]",
        "AGENDA_VIEW",
        "[1]",
    ] {
        let found = report_of(&with_env(list));
        assert!(found.contains("schema"), "{list}\n{found}");
    }
}

#[test]
fn restored_profiles_are_checked_the_same_way() {
    let (id, limits) = (ProfileId("personal".into()), Limits::default());
    let restore = |files: &Files| {
        let bundle = raw_bundle(files);
        from_bundle(&bundle, &id, &revision_of(&bundle), &limits)
    };
    assert!(restore(&with_env("[AGENDA_VIEW]")).is_ok());
    let found = restore(&with_env("[NOPE_KEY]")).unwrap_err();
    assert!(found.iter().any(|i| i.code == "env-missing"), "{found:?}");
}

#[test]
fn rejects_reserved_and_overlong_secret_env_names() {
    for name in [
        "PATH",
        "LD_PRELOAD",
        "NODE_OPTIONS",
        "HTTPS_PROXY",
        "BOT_EFFECT_URL",
    ] {
        let files = edit(
            base(),
            TOOLS_PATH,
            "CALENDAR_TOKEN: calendar-reader",
            &format!("{name}: calendar-reader"),
        );
        let found = report_of(&files);
        assert!(found.contains("env-reserved"), "{name}\n{found}");
    }
    let long_name = "A".repeat(129);
    let files = edit(
        base(),
        TOOLS_PATH,
        "CALENDAR_TOKEN: calendar-reader",
        &format!("{long_name}: calendar-reader"),
    );
    let found = report_of(&files);
    assert!(found.contains("schema"), "{found}");
}

#[test]
fn legacy_v1_bundle_with_reserved_or_overlong_secret_env_name_still_restores() {
    let (id, limits) = (ProfileId("personal".into()), Limits::default());
    let long_name = "A".repeat(129);
    for env_name in ["PATH", "NODE_OPTIONS", "BOT_EFFECT_URL", long_name.as_str()] {
        let files = edit(
            base(),
            TOOLS_PATH,
            "CALENDAR_TOKEN: calendar-reader",
            &format!("{env_name}: calendar-reader"),
        );
        // 현행(`Strict`) 프로필 검증은 반드시 거절한다.
        assert!(
            check(&files).is_err(),
            "{env_name}는 Strict에서 거절해야 해요"
        );
        // 이미 DB에 저장돼 있던 구판(`BOTPROFILE1`) 번들은 복원을 깨지 않고 복원된다(실행 차단은 브로커가 맡는다).
        let bundle = raw_bundle(&files);
        assert!(bundle.starts_with(b"BOTPROFILE1\n"));
        let restored = from_bundle(&bundle, &id, &revision_of(&bundle), &limits)
            .unwrap_or_else(|e| panic!("구판 BOTPROFILE1 번들({env_name}) 복원 실패: {e:?}"));
        assert!(
            list_tool(&restored, "main").secrets.contains_key(env_name),
            "{env_name}"
        );
    }
}
