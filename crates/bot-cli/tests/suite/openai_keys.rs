//! Process-level OpenAI key slot validation: malformed or duplicate key values stop both
//! `bot validate` and `bot serve` before publishing, without ever printing secret values.
use crate::process_support::*;
use std::path::Path;

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

struct Setup {
    env: Vec<(String, String)>,
    schema: Schema,
    _dir: tempfile::TempDir,
}

fn setup(build: &str, next: &str) -> Setup {
    let schema = Schema::create().expect("BOT_TEST_DATABASE_URL is required");
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
    let cases: Vec<(&str, String, String, &[&str])> = vec![
        (
            "too short",
            "short".into(),
            good("next"),
            &["ASCII", "32", "256"],
        ),
        (
            "contains whitespace",
            format!("has space {}", "x".repeat(40)),
            good("next"),
            &["ASCII", "32", "256"],
        ),
        (
            "duplicate value across slots",
            shared.clone(),
            shared,
            &["personal/openai", "build"],
        ),
    ];
    for (name, build, next, expected) in cases {
        let setup = setup(&build, &next);
        let validate = bot(&refs(&setup.env), &["validate"]);
        let serve = bot(
            &refs(&setup.env),
            &["serve", "--role", "gateway", "--activate"],
        );
        for (what, output) in [("validate", &validate), ("serve", &serve)] {
            let said = text(output);
            assert!(!output.status.success(), "{name} {what}: {said}");
            for needle in expected {
                assert!(said.contains(needle), "{name} {what}: {said}");
            }
            assert!(!said.contains("Validation passed"), "{name} {what}: {said}");
            for value in [&build, &next] {
                if value.len() > 12 {
                    assert!(
                        !said.contains(value.as_str()),
                        "{name} {what}: secret value leaked in output"
                    );
                }
            }
        }
        assert_eq!(
            setup.schema.active(),
            "",
            "{name}: failed startup must not publish anything"
        );
    }
}

#[test]
fn distinct_key_shaped_values_pass_the_validation() {
    let setup = setup(&good("build"), &good("next"));
    let validate = bot(&refs(&setup.env), &["validate"]);
    let said = text(&validate);
    assert!(validate.status.success(), "{said}");
    assert!(said.contains("Validation passed"), "{said}");
}
