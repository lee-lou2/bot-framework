//! Security-review regressions over real sockets for `bot mcp sync` (profile validation before connecting, diagnostic scrubbing, lock validation, and non-canonical numeric host rejection).
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
    McpTool::new("get-forecast", "City forecast", schema())
}

struct World {
    tmp: tempfile::TempDir,
    root: PathBuf,
    settings: Settings,
    skill: PathBuf,
    lock: PathBuf,
}

fn world(port: u16, auth: bool) -> World {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("profiles");
    copy_example_profile(&root);
    let skill = root.join("personal/agents/main/skills/weather");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(skill.join("SKILL.md"), "Forecast skill\n").unwrap();
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

fn untouched(server: &Served, secrets: &FakeSecrets, label: &str) {
    assert!(
        server.mcp.requests().is_empty(),
        "{label}: server was contacted: {:?}",
        server
            .mcp
            .requests()
            .iter()
            .map(|r| r.rpc())
            .collect::<Vec<_>>()
    );
    assert!(
        secrets.resolved().is_empty(),
        "{label}: secrets were resolved: {:?}",
        secrets.resolved()
    );
}

#[tokio::test]
async fn sync_connects_and_reads_secrets_only_after_the_profile_is_identified_like_validate_does() {
    let secrets = store();
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
        ("profile.yaml without id", |w| {
            let path = w.root.join("personal/profile.yaml");
            let text = std::fs::read_to_string(&path).unwrap();
            std::fs::write(path, text.replace("id: personal\n", "")).unwrap();
        }),
        ("id differing from folder", |w| {
            let path = w.root.join("personal/profile.yaml");
            let text = std::fs::read_to_string(&path).unwrap();
            std::fs::write(path, text.replace("id: personal\n", "id: other\n")).unwrap();
        }),
        ("schema-invalid profile.yaml", |w| {
            std::fs::write(
                w.root.join("personal/profile.yaml"),
                "version: 1\nid: personal\n",
            )
            .unwrap();
        }),
        ("missing profile.yaml", |w| {
            std::fs::remove_file(w.root.join("personal/profile.yaml")).unwrap();
        }),
        ("symlink inside profile", |w| {
            std::os::unix::fs::symlink("/etc/hosts", w.root.join("personal/knowledge/linked.md"))
                .unwrap();
        }),
        ("hardlink inside profile", |w| {
            std::fs::hard_link(
                w.root.join("personal/rules.md"),
                w.root.join("personal/knowledge/hard.md"),
            )
            .unwrap();
        }),
        ("unknown file", |w| {
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
    let secrets = store();
    let server = server_with(vec![forecast()]).await;
    let w = world(server.port, true);
    copy_dir(&w.root.join("personal"), &w.root.join("other"));
    let profile = w.root.join("other/profile.yaml");
    let text = std::fs::read_to_string(&profile).unwrap();
    std::fs::write(profile, text.replace("id: personal\n", "id: other\n")).unwrap();
    let linked = w.root.join("personal/agents/main/skills/linked");
    std::os::unix::fs::symlink(w.root.join("other/agents/main/skills/weather"), &linked).unwrap();
    let error = sync_in(&w, &linked, &secrets).await.expect_err("symlink");
    assert!(!error.contains("other-profile-token"), "{error}");
    untouched(&server, &secrets, "symlinked skill");
    std::os::unix::fs::symlink(w.root.join("personal"), w.root.join("alias")).unwrap();
    let aliased = w.root.join("alias/agents/main/skills/weather");
    let error = sync_in(&w, &aliased, &secrets).await.expect_err("alias");
    assert!(!error.is_empty());
    untouched(&server, &secrets, "profile alias");
    for dir in [
        w.root.join("personal/agents/main"),
        w.root.join("personal"),
        w.tmp.path().to_owned(),
    ] {
        sync_in(&w, &dir, &secrets)
            .await
            .expect_err(&dir.display().to_string());
    }
    untouched(&server, &secrets, "non-skill dir");
}

#[tokio::test]
async fn a_schema_diagnostic_never_repeats_text_the_server_chose() {
    for (label, schema, forbidden) in [
        (
            "unknown keyword",
            json!({"type": "object", "evil-keyword-xyz": 1}),
            "evil-keyword-xyz",
        ),
        (
            "external ref",
            json!({"type": "object", "properties": {"a": {"$ref": "https://evil.example/leak"}}}),
            "evil.example",
        ),
        (
            "pattern",
            json!({"type": "object", "properties": {"a": {"type": "string", "pattern": "(?<evil-name>x)"}}}),
            "evil-name",
        ),
        (
            "keyword matching token",
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
            "{label}: diagnostic echoed server text: {error}"
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
            "declared tool",
            vec![McpTool::new("get-forecast", "d", number.clone())],
        ),
        (
            "undeclared tool",
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
    // Exceeds the 64 KiB single-YAML-value cap that `validate` rejects with `yaml-too-large`.
    for size in [65_537, 70_000] {
        server.mcp.set_tools(vec![McpTool::new(
            "get-forecast",
            &"a".repeat(size),
            schema(),
        )]);
        let error = sync(&w, &secrets).await.expect_err("oversized description");
        assert!(error.contains("yaml-too-large"), "{size}: {error}");
        assert!(!error.contains("aaaa"), "{error}");
        assert_eq!(
            std::fs::read_to_string(&w.lock).unwrap(),
            baseline,
            "{size}"
        );
        assert_eq!(std::fs::metadata(&w.lock).unwrap().ino(), inode, "{size}");
    }
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
    // reqwest parses these notations as 127.0.0.1 and would connect without DNS resolution checks.
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
    assert_eq!(target.accepted().await, 0, "must not open any connection");
}
