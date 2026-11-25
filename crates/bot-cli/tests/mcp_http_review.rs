//! 보안 검토에서 나온 재현(실제 소켓): `bot mcp sync`는 프로필을 `validate`와 같은 로더로 확정한 뒤에만 접속하고, 서버가 준 글을 진단에 되풀이하지 않으며,
//! 쓰기 전에 만든 잠금을 `validate`와 같은 검사로 통과시키고, 전송은 비정규 숫자 표기 호스트를 연결 전에 거절한다.
mod mcp_http_support;

use bot_broker::{
    mcp::wire::{RemoteWire, WireFail, WireMethod, WireRequest},
    proxy::SystemResolver,
    testing::{Era, FakeMcp, FakeSecrets, McpTool},
};
use bot_cli::{mcp_sync, serve::remote::ReqwestWire, settings::Settings};
use bot_core::port::SecretStore;
use mcp_http_support::{
    Counter, Served, copy_dir, copy_example_profile, host_file, serve, settings,
};
use serde_json::{Value, json};
use std::{os::unix::fs::MetadataExt, path::PathBuf, sync::Arc};

const TOKEN: &str = "tok-review-1234";

fn schema() -> Value {
    json!({"type": "object", "properties": {"city": {"type": "string"}},
           "required": ["city"], "additionalProperties": false})
}

fn forecast() -> McpTool {
    McpTool::new("get-forecast", "도시의 예보", schema())
}

struct World {
    tmp: tempfile::TempDir,
    root: PathBuf,
    settings: Settings,
    skill: PathBuf,
    lock: PathBuf,
}

/// 진짜 프로필 `personal`(예시 프로필 복사)의 스킬 `weather`와 그 서버를 가리키는 호스트 설정.
fn world(port: u16, auth: bool) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("profiles");
    copy_example_profile(&root);
    let skill = root.join("personal/agents/main/skills/weather");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "예보 스킬\n").unwrap();
    std::fs::write(
        skill.join("tools.yaml"),
        format!(
            "version: 1\nserver: weather\ntools:\n  - id: get-forecast\n    class: read\n    scopes: [calendar.read]\n    egress: [\"127.0.0.1:{port}\"]\n"
        ),
    )
    .unwrap();
    let host = host_file(tmp.path(), port, auth);
    World {
        settings: settings(&root, &host),
        lock: skill.join("tools.lock.yaml"),
        skill,
        root,
        tmp,
    }
}

/// 자격 보관소(프로필별 이름공간)와 그것이 받은 조회들.
fn store() -> Arc<FakeSecrets> {
    Arc::new(FakeSecrets::new(&[
        ("personal", "weather-token", TOKEN),
        ("other", "weather-token", "other-profile-token-5678"),
    ]))
}

async fn sync_in(
    w: &World,
    dir: &std::path::Path,
    secrets: &Arc<FakeSecrets>,
) -> Result<(), String> {
    let secrets = Arc::clone(secrets);
    mcp_sync::run_with(
        &w.settings,
        dir,
        move || Ok(secrets as Arc<dyn SecretStore>),
    )
    .await
}

async fn sync(w: &World, secrets: &Arc<FakeSecrets>) -> Result<(), String> {
    sync_in(w, &w.skill, secrets).await
}

async fn server_with(tools: Vec<McpTool>) -> Served {
    serve(FakeMcp::new(Era::Modern, tools)).await
}

/// 접속도 자격 조회도 없었는가.
fn untouched(server: &Served, secrets: &FakeSecrets, label: &str) {
    assert!(
        server.mcp.requests().is_empty(),
        "{label}: 서버에 닿았어요: {:?}",
        server
            .mcp
            .requests()
            .iter()
            .map(|r| r.rpc())
            .collect::<Vec<_>>()
    );
    assert!(
        secrets.resolved().is_empty(),
        "{label}: 자격을 읽었어요: {:?}",
        secrets.resolved()
    );
}

#[tokio::test]
async fn sync_connects_and_reads_secrets_only_after_the_profile_is_identified_like_validate_does() {
    let secrets = store();
    // 대조: 올바른 프로필이면 그 프로필의 보관소에서만 읽고 접속한다
    let server = server_with(vec![forecast()]).await;
    let w = world(server.port, true);
    sync(&w, &secrets).await.unwrap();
    assert_eq!(
        secrets.resolved(),
        [("personal".to_owned(), "weather-token".to_owned())]
    );
    assert!(w.lock.is_file());

    type Spoil = fn(&World);
    let cases: Vec<(&str, Spoil)> = vec![
        ("id 없는 profile.yaml", |w| {
            let path = w.root.join("personal/profile.yaml");
            let text = std::fs::read_to_string(&path).unwrap();
            std::fs::write(path, text.replace("id: personal\n", "")).unwrap();
        }),
        ("폴더와 다른 id", |w| {
            let path = w.root.join("personal/profile.yaml");
            let text = std::fs::read_to_string(&path).unwrap();
            std::fs::write(path, text.replace("id: personal\n", "id: other\n")).unwrap();
        }),
        ("스키마에 어긋난 profile.yaml", |w| {
            std::fs::write(
                w.root.join("personal/profile.yaml"),
                "version: 1\nid: personal\n",
            )
            .unwrap();
        }),
        ("profile.yaml 없음", |w| {
            std::fs::remove_file(w.root.join("personal/profile.yaml")).unwrap();
        }),
        ("프로필 안의 링크", |w| {
            std::os::unix::fs::symlink("/etc/hosts", w.root.join("personal/knowledge/linked.md"))
                .unwrap();
        }),
        ("프로필 안의 하드링크", |w| {
            std::fs::hard_link(
                w.root.join("personal/rules.md"),
                w.root.join("personal/knowledge/hard.md"),
            )
            .unwrap();
        }),
        ("모르는 파일", |w| {
            std::fs::write(w.root.join("personal/stray.txt"), "x").unwrap();
        }),
    ];
    for (label, spoil) in cases {
        let secrets = store();
        let server = server_with(vec![forecast()]).await;
        let w = world(server.port, true);
        spoil(&w);
        let error = sync(&w, &secrets).await.expect_err(label);
        assert!(!error.contains(TOKEN), "{label}: {error}");
        untouched(&server, &secrets, label);
        assert!(!w.lock.exists(), "{label}");
    }
}

#[tokio::test]
async fn a_link_between_profiles_never_picks_the_other_profiles_secrets() {
    // `personal`의 스킬 폴더가 다른 프로필 `other`의 스킬 폴더를 가리키는 링크다(경로를 풀면 `other` 아래다)
    let secrets = store();
    let server = server_with(vec![forecast()]).await;
    let w = world(server.port, true);
    copy_dir(&w.root.join("personal"), &w.root.join("other"));
    let profile = w.root.join("other/profile.yaml");
    let text = std::fs::read_to_string(&profile).unwrap();
    std::fs::write(profile, text.replace("id: personal\n", "id: other\n")).unwrap();
    let linked = w.root.join("personal/agents/main/skills/linked");
    std::os::unix::fs::symlink(w.root.join("other/agents/main/skills/weather"), &linked).unwrap();
    let error = sync_in(&w, &linked, &secrets).await.expect_err("링크");
    assert!(!error.contains("other-profile-token"), "{error}");
    untouched(&server, &secrets, "링크 스킬");
    // 프로필 폴더 자체가 링크인 입구(루트 안의 별칭)도 같다
    std::os::unix::fs::symlink(w.root.join("personal"), w.root.join("alias")).unwrap();
    let aliased = w.root.join("alias/agents/main/skills/weather");
    let error = sync_in(&w, &aliased, &secrets).await.expect_err("별칭");
    assert!(!error.is_empty());
    untouched(&server, &secrets, "프로필 별칭");
    // 스킬 폴더가 아닌 곳·루트 밖
    for dir in [
        w.root.join("personal/agents/main"),
        w.root.join("personal"),
        w.tmp.path().to_owned(),
    ] {
        sync_in(&w, &dir, &secrets)
            .await
            .expect_err(&dir.display().to_string());
    }
    untouched(&server, &secrets, "스킬이 아닌 폴더");
}

#[tokio::test]
async fn a_schema_diagnostic_never_repeats_text_the_server_chose() {
    for (label, schema, forbidden) in [
        (
            "모르는 키워드",
            json!({"type": "object", "evil-keyword-xyz": 1}),
            "evil-keyword-xyz",
        ),
        (
            "외부 참조",
            json!({"type": "object", "properties": {"a": {"$ref": "https://evil.example/leak"}}}),
            "evil.example",
        ),
        (
            "패턴",
            json!({"type": "object", "properties": {"a": {"type": "string", "pattern": "(?<evil-name>x)"}}}),
            "evil-name",
        ),
        (
            "자격과 같은 키워드",
            json!({"type": "object", TOKEN: 1}),
            TOKEN,
        ),
    ] {
        let secrets = store();
        let server = server_with(vec![McpTool::new("get-forecast", "d", schema)]).await;
        let w = world(server.port, true);
        let error = sync(&w, &secrets).await.expect_err(label);
        assert!(
            !error.contains(forbidden),
            "{label}: 진단에 서버의 글이 있어요: {error}"
        );
        assert!(!error.contains(TOKEN), "{label}: {error}");
        assert!(!w.lock.exists(), "{label}");
    }
}

#[tokio::test]
async fn a_digits_only_bearer_in_a_number_token_is_never_written_into_the_lock() {
    let secrets = Arc::new(FakeSecrets::new(&[(
        "personal",
        "weather-token",
        "123456789012",
    )]));
    let number = json!({"type": "object", "properties": {"n": {"type": "integer", "minimum": 123_456_789_012_u64}}});
    for (label, tools) in [
        (
            "선언한 도구",
            vec![McpTool::new("get-forecast", "d", number.clone())],
        ),
        (
            "선언하지 않은 도구",
            vec![forecast(), McpTool::new("other", "d", number.clone())],
        ),
    ] {
        let server = server_with(tools).await;
        let w = world(server.port, true);
        let error = sync(&w, &secrets).await.expect_err(label);
        assert!(!error.contains("123456789012"), "{label}: {error}");
        assert!(!w.lock.exists(), "{label}");
    }
}

#[tokio::test]
async fn a_lock_that_validate_would_refuse_is_never_written_and_the_old_one_stays() {
    let secrets = store();
    let server = server_with(vec![forecast()]).await;
    let w = world(server.port, true);
    sync(&w, &secrets).await.unwrap();
    let baseline = std::fs::read_to_string(&w.lock).unwrap();
    let inode = std::fs::metadata(&w.lock).unwrap().ino();
    // YAML 값 하나의 한도(64 KiB)를 한 바이트 넘는 설명: 예전에는 쓰고 성공했지만 `validate`는 `yaml-too-large`로 거절한다
    for size in [65_537, 70_000] {
        server.mcp.set_tools(vec![McpTool::new(
            "get-forecast",
            &"a".repeat(size),
            schema(),
        )]);
        let error = sync(&w, &secrets).await.expect_err("긴 설명");
        assert!(error.contains("yaml-too-large"), "{size}: {error}");
        assert!(!error.contains("aaaa"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&w.lock).unwrap(),
            baseline,
            "{size}"
        );
        assert_eq!(std::fs::metadata(&w.lock).unwrap().ino(), inode, "{size}");
    }
    // 한도 안이면 쓰고, 쓴 잠금은 프로필 로더가 그대로 받는다
    server.mcp.set_tools(vec![McpTool::new(
        "get-forecast",
        &"a".repeat(60_000),
        schema(),
    )]);
    sync(&w, &secrets).await.unwrap();
    assert_ne!(std::fs::read_to_string(&w.lock).unwrap(), baseline);
}

#[tokio::test]
async fn the_wire_refuses_non_canonical_numeric_hosts_before_any_connection() {
    let target = Counter::new().await;
    let wire = ReqwestWire::new(Arc::new(SystemResolver)).unwrap();
    let request = |tls: bool, host: &str| WireRequest {
        method: WireMethod::Post,
        tls,
        host: host.into(),
        port: target.port,
        path: "/mcp".into(),
        headers: Vec::new(),
        body: b"{}".to_vec(),
        allow_private: false,
        timeout: None,
    };
    // reqwest의 URL 파서는 이 표기들을 127.0.0.1로 읽는다: 이름 풀이 검사를 거치지 않고 연결해 버린다
    for host in [
        "0x7f000001",
        "2130706433",
        "0177.0.0.1",
        "127.1",
        "0x7f.0.0.1",
        "127.0.0.1.",
        "::1",
    ] {
        let result = wire.send(request(true, host)).await;
        assert!(matches!(result, Err(WireFail::Unreached)), "https {host}");
        let result = wire.send(request(false, host)).await;
        assert!(matches!(result, Err(WireFail::Unreached)), "http {host}");
    }
    assert_eq!(target.accepted().await, 0, "연결을 만들지 않아요");
}
