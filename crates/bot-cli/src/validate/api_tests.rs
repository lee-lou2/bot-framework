//! API 직접 호출 별칭(`anthropic-api`·`openai-api`)의 서비스 키 점검: 키가 있는 별칭만 필요한 자격에 들고, 모델 VM은 `claude-cli` 별칭만 요구한다.

use super::*;
use crate::host;
use bot_gateway::testing::profile;

fn host_with(providers: &str) -> HostFile {
    host::parse(&format!(
        r#"{{"version": 1, "models": {{"personal": ["assistant", "plain"], "work": ["other"]}}{providers}}}"#
    ))
    .unwrap()
}

#[test]
fn the_api_aliases_need_their_keys_but_never_the_model_vm_and_a_keyless_local_server_needs_neither()
{
    let snapshot = profile("personal").build();
    let host = host_with(
        r#", "providers": {
            "assistant": {"kind": "anthropic-api", "model": "m", "endpoint": "https://api.example", "secret": "anthropic-key"},
            "plain": {"kind": "openai-api", "model": "m", "endpoint": "http://127.0.0.1:11434/v1"},
            "other": {"kind": "openai-api", "model": "m", "endpoint": "https://api.example/v1", "secret": "work-key"}}"#,
    );
    // 허용한 별칭 중 키가 있는 것(assistant)만이다: 키 없는 로컬 서버(plain)와 다른 프로필의 별칭(other)은 아니다
    assert_eq!(
        model_secrets(&snapshot, &host),
        BTreeSet::from(["anthropic-key".to_owned()])
    );
    let needed = needed_secrets(&snapshot, &host);
    assert!(
        needed.contains("anthropic-key") && !needed.contains("work-key"),
        "{needed:?}"
    );
    // API 직접 호출은 VM이 필요 없다(키가 있어도 `sandbox.apple`을 요구하지 않는다)
    assert!(missing_model_sandbox(&snapshot, &host).is_none());
    // claude-cli와 섞이면 claude-cli 별칭 때문에만 VM이 필요하다
    let mixed = host_with(
        r#", "providers": {
            "assistant": {"kind": "claude-cli", "model": "m", "endpoint": "https://api.example", "secret": "model-key"},
            "plain": {"kind": "openai-api", "model": "m", "endpoint": "https://api.example/v1", "secret": "openai-key"}}"#,
    );
    assert!(missing_model_sandbox(&snapshot, &mixed).is_some());
    assert_eq!(
        model_secrets(&snapshot, &mixed),
        BTreeSet::from(["model-key".to_owned(), "openai-key".to_owned()])
    );
}
