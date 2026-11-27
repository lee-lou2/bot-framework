use super::*;
use bot_core::mcp::definition_sha256;
use serde_json::json;

fn listed() -> Vec<Value> {
    vec![
        json!({"name": "zeta", "title": "ignored", "description": "last", "inputSchema": {"type": "object", "properties": {"b": {"type": "string"}}}}),
        json!({"name": "alpha", "description": "first", "inputSchema": {"type": "object"}}),
        json!({"name": "other", "description": "undeclared tool", "inputSchema": {"type": "object"}}),
    ]
}

fn ids(names: &[&str]) -> Vec<(String, String)> {
    names
        .iter()
        .map(|n| ((*n).to_owned(), (*n).to_owned()))
        .collect()
}

#[test]
fn only_declared_tools_are_recorded_in_a_stable_order_with_the_shared_hash() {
    let entries = entries_of(&ids(&["zeta", "alpha"]), &listed(), &[]).unwrap();
    assert_eq!(entries.keys().collect::<Vec<_>>(), ["alpha", "zeta"]);
    assert_eq!(
        entries["alpha"].sha256,
        definition_sha256("alpha", "first", &json!({"type": "object"})).unwrap()
    );
    let mut reversed = listed();
    reversed.reverse();
    let again = entries_of(&ids(&["alpha", "zeta"]), &reversed, &[]).unwrap();
    assert_eq!(
        lock::render("weather", &entries),
        lock::render("weather", &again)
    );
    assert!(!lock::render("weather", &entries).unwrap().contains("other"));
}

fn problems(declared: &[&str], listed: &[Value], credentials: &[Vec<u8>]) -> String {
    entries_of(&ids(declared), listed, credentials)
        .expect_err("expected failure")
        .join("\n")
}

#[test]
fn a_declared_tool_the_server_lacks_or_names_twice_fails_the_whole_sync() {
    assert!(problems(&["alpha", "missing"], &listed(), &[]).contains("`missing`"));
    let mut twice = listed();
    twice.push(json!({"name": "alpha", "description": "again", "inputSchema": {"type": "object"}}));
    let dup = problems(&["alpha"], &twice, &[]);
    assert!(
        dup.contains("duplicate") || dup.contains("more than one") || dup.contains("multiple"),
        "{dup}"
    );
    for broken in [
        json!({"name": "alpha", "inputSchema": null}),
        json!({"name": "alpha", "description": 5, "inputSchema": {"type": "object"}}),
    ] {
        assert!(problems(&["alpha"], &[broken], &[]).contains("inputSchema"));
    }
}

#[test]
fn schemas_the_profile_cannot_read_and_bad_header_annotations_are_refused() {
    let unsupported = json!({"name": "alpha", "inputSchema": {"type": "object", "patternProperties": {}, "uniqueItems": true}});
    assert!(problems(&["alpha"], &[unsupported], &[]).contains("unsupported-keyword"));
    let annotated =
        json!({"name": "alpha", "inputSchema": {"type": "object", "format": "x", "$schema": "s"}});
    assert!(entries_of(&ids(&["alpha"]), &[annotated], &[]).is_ok());
    let bad_header = json!({"name": "alpha", "inputSchema": {"type": "object",
        "properties": {"a": {"type": "number", "x-mcp-header": "A"}}}});
    assert!(problems(&["alpha"], &[bad_header], &[]).contains("x-mcp-header"));
    let good = json!({"name": "alpha", "inputSchema": {"type": "object",
        "properties": {"region": {"type": "string", "x-mcp-header": "Region"}}}});
    assert!(entries_of(&ids(&["alpha"]), &[good], &[]).is_ok());
}

#[test]
fn a_credential_echoed_into_any_part_of_a_definition_is_never_written() {
    let bearer = b"bearer-value-123456".to_vec();
    // Assemble token-shaped test string dynamically so secret scanners do not flag a literal token.
    let token = ["xoxb", "1234567890", "abcdef"].join("-");
    for tool in [
        json!({"name": "alpha", "description": "key is bearer-value-123456", "inputSchema": {"type": "object"}}),
        json!({"name": "alpha", "inputSchema": {"type": "object", "description": "bearer-value-123456"}}),
        json!({"name": "alpha", "inputSchema": {"type": "object", "properties": {"bearer-value-123456": {"type": "string"}}}}),
        json!({"name": "alpha", "inputSchema": {"type": "object", "properties": {"a": {"type": "string", "enum": ["bearer-value-123456"]}}}}),
        json!({"name": "alpha", "description": format!("token {token}"), "inputSchema": {"type": "object"}}),
    ] {
        let why = problems(
            &["alpha"],
            std::slice::from_ref(&tool),
            std::slice::from_ref(&bearer),
        );
        assert!(
            why.to_ascii_lowercase().contains("credential"),
            "{tool}: {why}"
        );
    }
    let clean =
        json!({"name": "alpha", "description": "no key", "inputSchema": {"type": "object"}});
    assert!(entries_of(&ids(&["alpha"]), &[clean], std::slice::from_ref(&bearer)).is_ok());
}

#[test]
fn the_file_is_replaced_atomically_and_never_through_a_link() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("tools.lock.yaml");
    write_atomic(&path, "first\n").unwrap();
    write_atomic(&path, "second\n").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().flatten().collect();
    assert_eq!(leftovers.len(), 1, "no temporary file left behind");
    let secret = tmp.path().join("secret.txt");
    std::fs::write(&secret, "original").unwrap();
    let linked = tmp.path().join("linked.yaml");
    std::os::unix::fs::symlink(&secret, &linked).unwrap();
    assert!(write_atomic(&linked, "overwrite").is_err());
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "original");
}

fn real_world() -> Value {
    json!({
        "name": "get_forecast", "description": "City forecast",
        "inputSchema": {
            "$schema": "http://json-schema.org/draft-07/schema#", "type": "object",
            "properties": {
                "city": {"$ref": "#/$defs/City"},
                "callback": {"type": "string", "format": "uri"}
            },
            "required": ["city"], "additionalProperties": false,
            "$defs": {"City": {"type": "string", "minLength": 1, "title": "City"}}
        }
    })
}

#[test]
fn a_declared_id_maps_to_the_server_name_and_the_hash_is_over_the_definition_as_served() {
    let served = real_world();
    let declared = vec![("get-forecast".to_owned(), "get_forecast".to_owned())];
    let entries = entries_of(&declared, std::slice::from_ref(&served), &[]).unwrap();
    let entry = &entries["get-forecast"];
    assert_eq!(entry.name, "get_forecast");
    assert_eq!(entry.input_schema, served["inputSchema"]);
    assert_eq!(
        entry.sha256,
        definition_sha256("get_forecast", "City forecast", &served["inputSchema"]).unwrap()
    );
    let text = lock::render("weather", &entries).unwrap();
    assert!(text.contains("id: \"get-forecast\"") && text.contains("name: \"get_forecast\""));
    let by_id = vec![("get-forecast".to_owned(), "get-forecast".to_owned())];
    let why = entries_of(&by_id, std::slice::from_ref(&served), &[]).expect_err("expected failure");
    assert!(why[0].contains("get-forecast"), "{why:?}");
}

#[test]
fn a_recursive_or_external_reference_in_a_served_schema_is_refused_with_the_reason() {
    for (schema, want) in [
        (
            json!({"type": "object", "properties": {"node": {"$ref": "#/$defs/Node"}},
                   "$defs": {"Node": {"type": "object", "properties": {"next": {"$ref": "#/$defs/Node"}}}}}),
            "[ref-recursive] /properties/node",
        ),
        (
            json!({"type": "object", "properties": {"x": {"$ref": "https://example.test/x.json"}}}),
            "[ref-external] /properties/x",
        ),
    ] {
        let tool = json!({"name": "alpha", "description": "d", "inputSchema": schema});
        let why = problems(&["alpha"], std::slice::from_ref(&tool), &[]);
        // Diagnostics must carry only fixed codes and JSON pointers, never server-chosen strings.
        assert!(why.contains(want), "{why}");
        assert!(
            !why.contains("example.test") && !why.contains("Node"),
            "{why}"
        );
    }
}

#[test]
fn a_diagnostic_never_repeats_what_the_server_chose_and_the_credential_check_comes_first() {
    let bearer = b"bearer-value-123456".to_vec();
    for (schema, want) in [
        (
            json!({"type": "object", "evil-keyword-xyz": 1}),
            "unsupported-keyword",
        ),
        (
            json!({"type": "object", "properties": {"a": {"$ref": "https://evil.example/x"}}}),
            "ref-external",
        ),
        (
            json!({"type": "object", "properties": {"a": {"type": "string", "pattern": "(?<evil-name>x)"}}}),
            "bad-pattern",
        ),
    ] {
        let tool = json!({"name": "alpha", "description": "d", "inputSchema": schema});
        let why = problems(
            &["alpha"],
            std::slice::from_ref(&tool),
            std::slice::from_ref(&bearer),
        );
        assert!(why.contains(want), "{want}: {why}");
        for echoed in ["evil-keyword-xyz", "evil.example", "evil-name"] {
            assert!(!why.contains(echoed), "{echoed}: {why}");
        }
    }
    // Credential leak detection must short-circuit before building any other diagnostic.
    let numeric = b"123456789012".to_vec();
    for (tools, secret) in [
        (
            vec![
                json!({"name": "alpha", "description": "d", "inputSchema": {"type": "object"}}),
                json!({"name": "other", "description": "key bearer-value-123456", "inputSchema": {"type": "object"}}),
            ],
            &bearer,
        ),
        (
            vec![
                json!({"name": "alpha", "description": "d", "inputSchema": {"type": "object", "bearer-value-123456": 1}}),
            ],
            &bearer,
        ),
        (
            vec![
                json!({"name": "alpha", "description": "d", "inputSchema": {"type": "object", "properties": {"n": {"type": "integer", "minimum": 123456789012_u64}}}}),
            ],
            &numeric,
        ),
    ] {
        let why = problems(&["alpha"], &tools, std::slice::from_ref(secret));
        assert!(why.to_ascii_lowercase().contains("credential"), "{why}");
        assert!(!why.contains("bearer-value-123456"), "{why}");
    }
}

#[tokio::test]
async fn invalid_skill_md_fails_before_reading_secrets_or_connecting_to_remote() {
    use crate::settings::SecretsKind;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connected = Arc::new(AtomicBool::new(false));
    let seen = connected.clone();
    let accept = tokio::spawn(async move {
        if listener.accept().await.is_ok() {
            seen.store(true, Ordering::SeqCst);
        }
    });

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("profiles");
    let skill_dir = root.join("demo/skills/docs");
    std::fs::create_dir_all(root.join("demo/agents/main")).unwrap();
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        root.join("demo/profile.yaml"),
        "version: 1\nid: demo\nmodel: assistant\nroles: {owner: [run, docs.read]}\nmembers: [{identity: \"local:owner\", roles: [owner]}]\nbindings: [{id: desk, adapter: cli, account: owner, agent: main}]\nsecrets: [docs-token]\n",
    )
    .unwrap();
    std::fs::write(root.join("demo/agents/main/AGENT.md"), "Reads documents.\n").unwrap();
    std::fs::write(
        skill_dir.join("tools.yaml"),
        format!(
            "version: 1\nserver: docs\ntools:\n  - id: read\n    class: read\n    scopes: [docs.read]\n    egress: [\"{addr}\"]\n"
        ),
    )
    .unwrap();
    let host_path = tmp.path().join("host.json");
    let uid = current_uid().unwrap();
    std::fs::write(
        &host_path,
        json!({
            "version": 1,
            "gateway": {"cli": {"accounts": {"owner": {"uid": uid}}}},
            "models": {"demo": ["assistant"]},
            "providers": {"assistant": {"kind": "demo"}},
            "servers": {
                "docs": {
                    "transport": "http",
                    "url": format!("http://{addr}/mcp"),
                    "auth": {"kind": "bearer", "secret": "docs-token"}
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let settings = Settings {
        profiles: Some(root),
        host_config: Some(host_path),
        database_url: None,
        db_max_connections: None,
        socket: None,
        secrets: SecretsKind::Env,
        secrets_reread: None,
        installs: None,
        admin_token_file: None,
    };

    let over = "x".repeat(bot_profile::SKILL_MAX_BYTES + 1);
    for (label, content, want) in [
        ("over 64 KiB", over.as_str(), "[skill-too-large]"),
        (
            "allowed-tools forbidden",
            "---\nname: docs\nallowed-tools: Bash\n---\nbody\n",
            "[skill-allowed-tools]",
        ),
        ("unclosed --- fence", "---", "[front-matter-unclosed]"),
        (
            "directory name mismatch",
            "---\nname: other\n---\nbody\n",
            "[skill-name-mismatch]",
        ),
    ] {
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
        let secret_called = Arc::new(AtomicBool::new(false));
        let flag = secret_called.clone();
        let err = run_with(&settings, &skill_dir, move || {
            flag.store(true, Ordering::SeqCst);
            Err("secret store must not be touched".to_owned())
        })
        .await
        .expect_err(label);
        assert!(err.contains(want), "{label}: {err}");
        assert!(
            !secret_called.load(Ordering::SeqCst),
            "{label}: read secrets despite SKILL.md error"
        );
        assert!(
            !connected.load(Ordering::SeqCst),
            "{label}: connected to remote despite SKILL.md error"
        );
        assert!(!skill_dir.join(lock::FILE).exists(), "{label}");
    }
    accept.abort();
}

#[test]
fn write_atomic_never_unlinks_an_existing_tmp_file_on_already_exists() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("tools.lock.yaml");
    let pre = dir.path().join(".tools.lock.yaml.tmp-fixed");
    std::fs::write(&pre, "precious").unwrap();
    let err = write_atomic_with(&target, "new-lock", || "fixed".to_owned()).unwrap_err();
    assert!(err.to_ascii_lowercase().contains("write"), "{err}");
    assert_eq!(std::fs::read_to_string(&pre).unwrap(), "precious");
    assert!(!target.exists());
    write_atomic(&target, "ok-lock").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "ok-lock");
}
