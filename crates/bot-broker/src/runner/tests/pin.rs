//! Pinned tool definition hashes (`tools.lock.yaml`) are verified on local stdio servers before `tools/call`.

use super::*;
use bot_core::mcp::definition_sha256;

fn pin_of(description: &str, schema: &Value) -> Option<String> {
    definition_sha256("delete", description, schema)
}

fn pinned(description: &str) -> Tool {
    let mut tool = decl(&[], &[], false);
    // `Server::new` advertises `"x"` as the tool description.
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
    // Description mismatch with identical schema blocks tool poisoning.
    let r = plain();
    let ran = run(&r, job(pinned("other description"), r#"{"event":"e1"}"#)).await;
    assert!(
        matches!(ran, Ran::Refused("tool_definition_changed")),
        "{ran:?}"
    );
    assert!(r.sandbox.calls().is_empty(), "must not send tools/call");
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
    let r = rig(Server::new(&[("other", schema())]), |_| {}, |_| {});
    let ran = run(&r, job(pinned("x"), r#"{"event":"e1"}"#)).await;
    assert!(matches!(ran, Ran::Refused("tool_missing")), "{ran:?}");
    assert!(r.sandbox.calls().is_empty());
}

#[tokio::test]
async fn an_inline_tool_keeps_the_old_schema_only_check() {
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
