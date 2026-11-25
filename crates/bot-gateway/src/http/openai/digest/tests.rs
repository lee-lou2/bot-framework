use super::*;
use crate::http::openai::request;

fn digest_of(json: &str) -> [u8; 32] {
    let chat = request::parse(json.as_bytes()).expect("valid request");
    semantic(&chat.model, &chat.body)
}

const BASE: &str = r#"{"model":"assistant","messages":[{"role":"system","content":"s"},{"role":"user","content":"hi","name":"kim"}]}"#;

#[test]
fn the_same_meaning_gives_the_same_digest_however_it_is_spelled() {
    let spaced = r#"{ "messages": [ {"content":"s","role":"system"}, {"name":"kim","content":"hi","role":"user"} ], "model": "assistant" }"#;
    assert_eq!(digest_of(BASE), digest_of(spaced));
    // 내용 파트 배열은 이어 붙인 같은 글이다
    let parts = r#"{"model":"assistant","messages":[{"role":"system","content":[{"type":"text","text":"s"}]},{"role":"user","content":"hi","name":"kim"}]}"#;
    assert_eq!(digest_of(BASE), digest_of(parts));
}

#[test]
fn presentation_and_ignored_fields_do_not_change_it() {
    let base = digest_of(BASE);
    let streamed = r#"{"model":"assistant","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"system","content":"s"},{"role":"user","content":"hi","name":"kim"}]}"#;
    assert_eq!(base, digest_of(streamed));
    let ignored = r#"{"model":"assistant","temperature":0.2,"top_p":0.5,"seed":7,"user":"u","metadata":{"a":"b"},"store":false,"n":1,"response_format":{"type":"text"},"messages":[{"role":"system","content":"s"},{"role":"user","content":"hi","name":"kim"}]}"#;
    assert_eq!(base, digest_of(ignored));
}

#[test]
fn anything_that_changes_the_work_changes_the_digest() {
    let base = digest_of(BASE);
    let variants = [
        // 모델
        r#"{"model":"other","messages":[{"role":"system","content":"s"},{"role":"user","content":"hi","name":"kim"}]}"#,
        // 내용
        r#"{"model":"assistant","messages":[{"role":"system","content":"s"},{"role":"user","content":"hi!","name":"kim"}]}"#,
        // 역할
        r#"{"model":"assistant","messages":[{"role":"developer","content":"s"},{"role":"user","content":"hi","name":"kim"}]}"#,
        // 이름
        r#"{"model":"assistant","messages":[{"role":"system","content":"s"},{"role":"user","content":"hi","name":"lee"}]}"#,
        r#"{"model":"assistant","messages":[{"role":"system","content":"s"},{"role":"user","content":"hi"}]}"#,
        // 순서
        r#"{"model":"assistant","messages":[{"role":"user","content":"hi","name":"kim"},{"role":"system","content":"s"}]}"#,
        // 턴 수
        r#"{"model":"assistant","messages":[{"role":"system","content":"s"},{"role":"user","content":"hi","name":"kim"},{"role":"assistant","content":"x"}]}"#,
    ];
    for variant in variants {
        assert_ne!(base, digest_of(variant), "{variant}");
    }
}

#[test]
fn turn_boundaries_cannot_be_forged_by_moving_text_between_turns() {
    let a = r#"{"model":"m","messages":[{"role":"user","content":"ab"},{"role":"user","content":"c"}]}"#;
    let b = r#"{"model":"m","messages":[{"role":"user","content":"a"},{"role":"user","content":"bc"}]}"#;
    assert_ne!(digest_of(a), digest_of(b));
}

#[test]
fn the_canonical_form_is_pinned_so_stored_digests_keep_meaning_across_releases() {
    // 저장소는 접수 때의 지문을 보관해 같은 키의 재전달을 이것과 견준다. 정규형이 바뀌면 옛 키의 재조회가 전부 충돌이 된다 — 바꿀 때는 `VERSION`을 올리고
    // 옛 지문의 처리를 따로 정해야 한다.
    let hex: String = digest_of(BASE).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        "edb67d1f01474c45553695a4d38c7f1a35056e2904fdb45198343919f5ed03d0"
    );
}
