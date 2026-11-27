//! tools.yaml `timeout_secs`(도구 호출 시간 한도, 초): 스키마(1~900의 정수), 도구 판본에 드는지, 호스트 천장 대조(`tool-timeout-over-cap`).
//! 선언하지 않은 도구의 실효 한도는 60초이고, 천장만 올려도 그대로다. 천장을 정하지 않은 호스트의 천장도 60초다.

use crate::support::{Files, TOOLS_PATH, Temp, agent, base, check, edit, host, write_profile};
use bot_core::{
    id::ToolId,
    profile::{Snapshot, Tool},
};
use bot_profile::{Limits, ProfileSet, Report, load};

/// 기본 프로필의 `agenda.list`에 `timeout_secs: <값>`을 붙인다(값은 YAML 그대로).
fn with_timeout(value: &str) -> Files {
    edit(
        base(),
        TOOLS_PATH,
        "egress: [\"calendar.example:443\"]\n",
        &format!("egress: [\"calendar.example:443\"]\n    timeout_secs: {value}\n"),
    )
}

fn list_tool(snap: &Snapshot) -> &Tool {
    &snap.agents[&agent("main")].skills["agenda"].tools[&ToolId("agenda.list".into())]
}

/// 호스트 천장(`limits.tool_call_max_secs`)을 정해 읽는다. `None`이면 천장을 정하지 않은 호스트(기본 60초)다.
fn load_with(files: &Files, ceiling: Option<u32>) -> Result<ProfileSet, Report> {
    let root = Temp::new("timeout");
    write_profile(root.path(), "personal", files);
    let mut host = host();
    host.tool_call_max_secs = ceiling;
    load(root.path(), &host, &Limits::default())
}

/// 천장을 최대(900초)로 둔 호스트에서의 오류 코드들(스키마·참조 검사만 보려는 시험용).
fn codes(files: &Files) -> Vec<&'static str> {
    match load_with(files, Some(900)) {
        Ok(_) => Vec::new(),
        Err(report) => report.codes(),
    }
}

/// 천장을 최대로 둔 호스트에서 읽은 `personal`의 스냅샷.
fn snapshot(files: &Files) -> Snapshot {
    let set = load_with(files, Some(900)).unwrap_or_else(|report| panic!("{report}"));
    (**set
        .profile(&bot_core::id::ProfileId("personal".into()))
        .unwrap())
    .clone()
}

fn over_cap(files: &Files, ceiling: Option<u32>) -> bool {
    match load_with(files, ceiling) {
        Ok(_) => false,
        Err(report) => {
            assert!(report.has("tool-timeout-over-cap"), "{report}");
            true
        }
    }
}

#[test]
fn only_whole_seconds_from_one_to_nine_hundred_are_accepted_by_the_schema() {
    for ok in ["1", "60", "300", "900"] {
        assert!(
            codes(&with_timeout(ok)).is_empty(),
            "{ok}: {:?}",
            codes(&with_timeout(ok))
        );
    }
    for bad in ["0", "901", "-1", "1.5", "\"30\"", "true", "null", "[60]"] {
        assert_eq!(codes(&with_timeout(bad)), ["schema"], "{bad}");
    }
}

#[test]
fn a_declared_limit_reaches_the_tool_and_binds_into_its_revision_only_when_declared() {
    let plain = snapshot(&base());
    assert_eq!(list_tool(&plain).timeout_secs, None);
    assert_eq!(list_tool(&plain).call_secs(), 60, "선언하지 않으면 60초");
    let long = snapshot(&with_timeout("600"));
    assert_eq!(list_tool(&long).timeout_secs, Some(600));
    assert_eq!(list_tool(&long).call_secs(), 600);
    // 한도를 선언하면 도구 판본이 달라지고(옛 승인이 새 한도에 쓰이지 않는다) 값이 다르면 판본도 다르다
    assert_ne!(list_tool(&long).revision, list_tool(&plain).revision);
    assert_ne!(
        list_tool(&long).revision,
        list_tool(&snapshot(&with_timeout("601"))).revision
    );
    assert_eq!(
        list_tool(&long).revision,
        list_tool(&snapshot(&with_timeout("600"))).revision,
        "같은 선언은 같은 판본"
    );
    // 다른 도구의 판본은 그대로다(선언하지 않은 도구의 판본은 이 기능이 생기기 전과 같다)
    let other = |snap: &Snapshot| {
        snap.agents[&agent("main")].skills["agenda"].tools[&ToolId("agenda.delete".into())]
            .revision
            .clone()
    };
    assert_eq!(other(&long), other(&plain));
}

#[test]
fn a_declared_limit_above_the_host_ceiling_refuses_the_activation_and_names_the_tool() {
    // 천장을 정하지 않은 호스트(기본 60초)
    assert!(!over_cap(&with_timeout("60"), None));
    assert!(over_cap(&with_timeout("61"), None));
    let report = format!("{}", check(&with_timeout("900")).unwrap_err());
    assert!(report.contains("agenda.list"), "{report}");
    assert!(report.contains("900초"), "{report}");
    assert!(report.contains("limits.tool_call_max_secs"), "{report}");
    // 천장을 올리면 그 안의 선언은 통과한다
    assert!(!over_cap(&with_timeout("900"), Some(900)));
    assert!(!over_cap(&with_timeout("300"), Some(300)));
    assert!(over_cap(&with_timeout("301"), Some(300)));
}

#[test]
fn an_undeclared_tool_counts_as_sixty_seconds_so_a_lower_ceiling_refuses_it_and_a_higher_one_does_not_change_it()
 {
    // 천장만 올려도 선언하지 않은 도구는 통과하고 한도는 60초 그대로다
    for ceiling in [None, Some(60), Some(300), Some(900)] {
        let set = load_with(&base(), ceiling).unwrap_or_else(|r| panic!("{ceiling:?}: {r}"));
        let snap = set
            .profile(&bot_core::id::ProfileId("personal".into()))
            .unwrap();
        assert_eq!(list_tool(snap).call_secs(), 60, "{ceiling:?}");
    }
    // 천장이 60초보다 낮으면 선언하지 않은 도구(실효 60초)도 거절하고, 고치는 길을 알려 준다
    assert!(over_cap(&base(), Some(30)));
    let report = format!("{}", load_with(&base(), Some(30)).unwrap_err());
    assert!(report.contains("기본 60초"), "{report}");
    assert!(report.contains("`timeout_secs`"), "{report}");
    // 천장 이하를 선언한 도구만 통과한다(다른 도구는 여전히 60초라 같이 고쳐야 한다)
    let mut all = base();
    for id in ["list", "delete", "find"] {
        all = edit(
            all,
            TOOLS_PATH,
            &format!("  - id: {id}\n"),
            &format!("  - id: {id}\n    timeout_secs: 30\n"),
        );
    }
    assert!(!over_cap(&all, Some(30)));
}
