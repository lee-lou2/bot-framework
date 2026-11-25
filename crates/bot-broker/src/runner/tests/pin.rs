//! `tools.lock.yaml`로 고정한 도구 정의(해시)는 로컬(stdio) 서버에도 같은 대조를 건다: 서버가 내세우는 이름·설명·입력 스키마가 해시와 다르면 호출하기 전에 막는다.
use super::*;
use bot_core::mcp::definition_sha256;

fn pin_of(description: &str, schema: &Value) -> Option<String> {
    definition_sha256("delete", description, schema)
}

fn pinned(description: &str) -> Tool {
    let mut tool = decl(&[], &[], false);
    // 가짜 서버(`Server::new`)는 설명을 "x"로 내세운다
    tool.definition = pin_of(description, &schema());
    tool
}

#[tokio::test]
async fn a_matching_definition_runs_and_a_changed_one_never_gets_a_call() {
    let r = plain();
    assert!(matches!(
        run(&r, job(pinned("x"), r#"{"event":"e1"}"#)).await,
        Ran::Done(_)
    ));
    // 설명만 다르다(스키마는 같다): 도구 오염을 막는다
    let r = plain();
    let ran = run(&r, job(pinned("다른 설명"), r#"{"event":"e1"}"#)).await;
    assert!(
        matches!(ran, Ran::Refused("tool_definition_changed")),
        "{ran:?}"
    );
    assert!(r.sandbox.calls().is_empty(), "tools/call을 보내지 않았다");
    // 서버가 다른 스키마를 내세운다
    let r = rig(
        Server::new(&[("delete", json!({"type": "object"}))]),
        |_| {},
        |_| {},
    );
    let ran = run(&r, job(pinned("x"), r#"{"event":"e1"}"#)).await;
    assert!(
        matches!(ran, Ran::Refused("tool_definition_changed")),
        "{ran:?}"
    );
    assert!(r.sandbox.calls().is_empty());
    // 서버에서 도구가 사라졌다
    let r = rig(Server::new(&[("other", schema())]), |_| {}, |_| {});
    let ran = run(&r, job(pinned("x"), r#"{"event":"e1"}"#)).await;
    assert!(matches!(ran, Ran::Refused("tool_missing")), "{ran:?}");
    assert!(r.sandbox.calls().is_empty());
}

#[tokio::test]
async fn an_inline_tool_keeps_the_old_schema_only_check() {
    // 해시가 없는 도구: 설명이 달라도 실행하고, 스키마가 다르면 보내지 않는다(전과 같다)
    let r = plain();
    assert!(matches!(
        run(&r, job(decl(&[], &[], false), r#"{"event":"e1"}"#)).await,
        Ran::Done(_)
    ));
    let r = rig(
        Server::new(&[("delete", json!({"type": "object"}))]),
        |_| {},
        |_| {},
    );
    assert!(matches!(
        run(&r, job(decl(&[], &[], false), r#"{"event":"e1"}"#)).await,
        Ran::NotSent
    ));
    assert!(r.sandbox.calls().is_empty());
}
