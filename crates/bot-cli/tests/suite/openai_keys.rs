//! OpenAI 호환 입구의 키 슬롯 점검(실제 프로세스·팀 시험 DB): 키 모양이 아니거나 슬롯 둘이 같은 값을 쓰면 `bot validate`와 `bot serve`가 시작 전에 거절하고
//! 아무것도 게시하지 않는다. 값은 어디에도 찍히지 않는다.
use crate::process_support::*;
use std::path::Path;

/// 프로필에 OpenAI 모델 바인딩(`assistant`)과 그 입구의 HTTP 멤버를 더한 복사본
fn with_openai_model(from: &Path, to: &Path) {
    copy_dir(from, to);
    let yaml_path = to.join("personal/profile.yaml");
    let yaml = std::fs::read_to_string(&yaml_path)
        .unwrap()
        .replace(
            "  - {id: desk, adapter: cli, account: owner, agent: main}",
            "  - {id: desk, adapter: cli, account: owner, agent: main}\n  - {id: assistant, adapter: http, endpoint: openai, model: assistant, agent: main}",
        )
        .replace(
            "  - {identity: \"local:owner\", roles: [owner]}",
            "  - {identity: \"local:owner\", roles: [owner]}\n  - {identity: \"http:openai/build\", roles: [reader]}",
        );
    std::fs::write(yaml_path, yaml).unwrap();
}

const HOST_HTTP: &str = r#", "http": {"listen": "127.0.0.1:0", "endpoints": {"openai": {"kind": "openai", "keys": {
    "build": {"secret": "build-key", "subject": "build"},
    "next": {"secret": "next-key", "subject": "build"}}}}}"#;

/// 값 둘을 슬롯 `build`·`next`의 자격으로 채운 환경(프로필·호스트 설정·팀 시험 DB)
struct Setup {
    env: Vec<(String, String)>,
    schema: Schema,
    _dir: tempfile::TempDir,
}

fn setup(build: &str, next: &str) -> Setup {
    let schema = Schema::create().expect("BOT_TEST_DATABASE_URL이 필요해요");
    let dir = tempfile::Builder::new()
        .prefix("k")
        .tempdir_in("/tmp")
        .unwrap();
    let profiles = dir.path().join("profiles");
    with_openai_model(&examples(), &profiles);
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let host = dir.path().join("host.json");
    std::fs::write(&host, host_json(my_uid(), &pkg, HOST_HTTP, "")).unwrap();
    let url = schema.url();
    let socket = dir.path().join("run").join("g.sock");
    let env = env_for(
        &profiles,
        &url,
        &socket,
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_SECRET__personal__build_key", build),
            ("BOT_SECRET__personal__next_key", next),
        ],
    );
    Setup {
        env,
        schema,
        _dir: dir,
    }
}

fn good(tag: &str) -> String {
    format!("{tag}-{}", "k".repeat(40))
}

#[test]
fn key_slots_that_are_not_key_shaped_or_that_share_a_value_stop_the_start_and_the_validation() {
    let shared = good("shared");
    let cases: Vec<(&str, String, String, &str)> = vec![
        ("너무 짧은 값", "short".into(), good("next"), "키 모양"),
        (
            "공백이 든 값",
            format!("has space {}", "x".repeat(40)),
            good("next"),
            "키 모양",
        ),
        ("슬롯 둘이 같은 값", shared.clone(), shared, "같은 값"),
    ];
    for (name, build, next, expect) in cases {
        let setup = setup(&build, &next);
        // 시작은 실패해 곧바로 끝난다(성공하면 서버가 떠서 끝나지 않으므로 거절이 기대일 때만 돌린다)
        let validate = bot(&refs(&setup.env), &["validate"]);
        let serve = bot(
            &refs(&setup.env),
            &["serve", "--role", "gateway", "--activate"],
        );
        for (what, output) in [("validate", &validate), ("serve", &serve)] {
            let said = text(output);
            assert!(!output.status.success(), "{name} {what}: {said}");
            assert!(said.contains(expect), "{name} {what}: {said}");
            assert!(
                !said.contains("입구·자격 점검 통과"),
                "{name} {what}: {said}"
            );
            // 값은 어디에도 찍지 않는다
            for value in [&build, &next] {
                if value.len() > 12 {
                    assert!(
                        !said.contains(value.as_str()),
                        "{name} {what}: 값이 출력에 나왔어요"
                    );
                }
            }
        }
        assert_eq!(
            setup.schema.active(),
            "",
            "{name}: 실패한 시작이 아무것도 게시하지 않아요"
        );
    }
}

#[test]
fn distinct_key_shaped_values_pass_the_validation() {
    // 통과하는 경우는 `validate`만 돌린다(`serve`는 끝나지 않는 서버다)
    let setup = setup(&good("build"), &good("next"));
    let validate = bot(&refs(&setup.env), &["validate"]);
    let said = text(&validate);
    assert!(validate.status.success(), "{said}");
    assert!(said.contains("입구·자격 점검 통과"), "{said}");
}
