use super::*;
use bot_core::mcp::definition_sha256;
use serde_json::json;

fn listed() -> Vec<Value> {
    vec![
        json!({"name": "zeta", "title": "무시", "description": "마지막", "inputSchema": {"type": "object", "properties": {"b": {"type": "string"}}}}),
        json!({"name": "alpha", "description": "처음", "inputSchema": {"type": "object"}}),
        json!({"name": "other", "description": "선언하지 않은 도구", "inputSchema": {"type": "object"}}),
    ]
}

/// id와 서버 이름이 같은 선언.
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
        definition_sha256("alpha", "처음", &json!({"type": "object"})).unwrap()
    );
    // 서버가 목록 순서를 바꿔도 같은 결과다
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
        .expect_err("실패해야 한다")
        .join("\n")
}

#[test]
fn a_declared_tool_the_server_lacks_or_names_twice_fails_the_whole_sync() {
    assert!(problems(&["alpha", "missing"], &listed(), &[]).contains("`missing`"));
    let mut twice = listed();
    twice.push(json!({"name": "alpha", "description": "또", "inputSchema": {"type": "object"}}));
    assert!(problems(&["alpha"], &twice, &[]).contains("둘 이상"));
    // 모양이 틀린 정의
    for broken in [
        json!({"name": "alpha", "inputSchema": null}),
        json!({"name": "alpha", "description": 5, "inputSchema": {"type": "object"}}),
    ] {
        assert!(problems(&["alpha"], &[broken], &[]).contains("모양"));
    }
}

#[test]
fn schemas_the_profile_cannot_read_and_bad_header_annotations_are_refused() {
    // 프로필 어휘 밖의 키워드(`uniqueItems`·`patternProperties`)는 거절하고, 주석 키워드(`format`·`$schema`)는 받는다
    let unsupported = json!({"name": "alpha", "inputSchema": {"type": "object", "patternProperties": {}, "uniqueItems": true}});
    assert!(problems(&["alpha"], &[unsupported], &[]).contains("어휘"));
    let annotated =
        json!({"name": "alpha", "inputSchema": {"type": "object", "format": "x", "$schema": "s"}});
    assert!(entries_of(&ids(&["alpha"]), &[annotated], &[]).is_ok());
    let bad_header = json!({"name": "alpha", "inputSchema": {"type": "object",
        "properties": {"a": {"type": "number", "x-mcp-header": "A"}}}});
    assert!(problems(&["alpha"], &[bad_header], &[]).contains("x-mcp-header"));
    // 올바른 주석은 받는다
    let good = json!({"name": "alpha", "inputSchema": {"type": "object",
        "properties": {"region": {"type": "string", "x-mcp-header": "Region"}}}});
    assert!(entries_of(&ids(&["alpha"]), &[good], &[]).is_ok());
}

#[test]
fn a_credential_echoed_into_any_part_of_a_definition_is_never_written() {
    let bearer = b"bearer-value-123456".to_vec();
    // 토큰 모양의 합성 값(소스에 한 덩어리로 두지 않는다: 비밀 검사기가 실제 키로 읽는다)
    let token = ["xoxb", "1234567890", "abcdef"].join("-");
    for tool in [
        json!({"name": "alpha", "description": "키는 bearer-value-123456 입니다", "inputSchema": {"type": "object"}}),
        json!({"name": "alpha", "inputSchema": {"type": "object", "description": "bearer-value-123456"}}),
        json!({"name": "alpha", "inputSchema": {"type": "object", "properties": {"bearer-value-123456": {"type": "string"}}}}),
        json!({"name": "alpha", "inputSchema": {"type": "object", "properties": {"a": {"type": "string", "enum": ["bearer-value-123456"]}}}}),
        json!({"name": "alpha", "description": format!("토큰 {token}"), "inputSchema": {"type": "object"}}),
    ] {
        let why = problems(
            &["alpha"],
            std::slice::from_ref(&tool),
            std::slice::from_ref(&bearer),
        );
        assert!(why.contains("자격"), "{tool}: {why}");
    }
    // 자격이 없으면 같은 정의는 통과한다(검사가 값에 반응한다는 뜻)
    let clean =
        json!({"name": "alpha", "description": "키 없음", "inputSchema": {"type": "object"}});
    assert!(entries_of(&ids(&["alpha"]), &[clean], std::slice::from_ref(&bearer)).is_ok());
}

#[test]
fn the_file_is_replaced_atomically_and_never_through_a_link() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("tools.lock.yaml");
    write_atomic(&path, "첫 번째\n").unwrap();
    write_atomic(&path, "두 번째\n").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "두 번째\n");
    let leftovers: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().flatten().collect();
    assert_eq!(leftovers.len(), 1, "임시 파일이 남지 않는다");
    // 링크 뒤의 파일은 건드리지 않는다
    let secret = tmp.path().join("secret.txt");
    std::fs::write(&secret, "원본").unwrap();
    let linked = tmp.path().join("linked.yaml");
    std::os::unix::fs::symlink(&secret, &linked).unwrap();
    assert!(write_atomic(&linked, "덮어쓰기").is_err());
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "원본");
}

/// 실제 서버처럼 snake_case 이름에 `$schema`·`format`·`$defs`·`$ref`가 든 정의.
fn real_world() -> Value {
    json!({
        "name": "get_forecast", "description": "도시의 예보",
        "inputSchema": {
            "$schema": "http://json-schema.org/draft-07/schema#", "type": "object",
            "properties": {
                "city": {"$ref": "#/$defs/City"},
                "callback": {"type": "string", "format": "uri"}
            },
            "required": ["city"], "additionalProperties": false,
            "$defs": {"City": {"type": "string", "minLength": 1, "title": "도시"}}
        }
    })
}

#[test]
fn a_declared_id_maps_to_the_server_name_and_the_hash_is_over_the_definition_as_served() {
    let served = real_world();
    let declared = vec![("get-forecast".to_owned(), "get_forecast".to_owned())];
    let entries = entries_of(&declared, std::slice::from_ref(&served), &[]).unwrap();
    // 잠금은 id로 묶고 서버 이름을 적는다. 해시는 서버가 준 {name, description, inputSchema} 그대로다($defs·$ref도 펼치지 않은 원본)
    let entry = &entries["get-forecast"];
    assert_eq!(entry.name, "get_forecast");
    assert_eq!(entry.input_schema, served["inputSchema"]);
    assert_eq!(
        entry.sha256,
        definition_sha256("get_forecast", "도시의 예보", &served["inputSchema"]).unwrap()
    );
    let text = lock::render("weather", &entries).unwrap();
    assert!(text.contains("id: \"get-forecast\"") && text.contains("name: \"get_forecast\""));
    // id로 찾으려 하면 서버에는 그 이름이 없다
    let by_id = vec![("get-forecast".to_owned(), "get-forecast".to_owned())];
    let why = entries_of(&by_id, std::slice::from_ref(&served), &[]).expect_err("실패해야 한다");
    assert!(
        why[0].contains("get-forecast") && why[0].contains("없어요"),
        "{why:?}"
    );
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
        // 고정 코드와 JSON 포인터만 싣는다(참조한 주소·정의 이름 같은 서버의 글은 싣지 않는다)
        assert!(
            why.contains(want) && why.contains("읽을 수 없어요"),
            "{why}"
        );
        assert!(
            !why.contains("example.test") && !why.contains("Node"),
            "{why}"
        );
    }
}

#[test]
fn a_diagnostic_never_repeats_what_the_server_chose_and_the_credential_check_comes_first() {
    let bearer = b"bearer-value-123456".to_vec();
    // 서버가 고른 키워드·참조·패턴은 진단에 되풀이되지 않는다(고정 코드와 JSON 포인터만)
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
    // 자격이 응답 어디에 있든(선언하지 않은 도구·키·숫자 표기) 다른 진단을 만들기 전에 멈춘다
    let numeric = b"123456789012".to_vec();
    for (tools, secret) in [
        (
            vec![
                json!({"name": "alpha", "description": "d", "inputSchema": {"type": "object"}}),
                json!({"name": "other", "description": "키 bearer-value-123456", "inputSchema": {"type": "object"}}),
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
        assert!(why.contains("자격"), "{why}");
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
    std::fs::write(root.join("demo/agents/main/AGENT.md"), "문서를 읽는다.\n").unwrap();
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
        ("단일 32 KiB 초과", over.as_str(), "[skill-too-large]"),
        (
            "allowed-tools 금지",
            "---\nname: docs\nallowed-tools: Bash\n---\n본문\n",
            "[skill-allowed-tools]",
        ),
        ("단독 --- 파일 끝", "---", "[front-matter-unclosed]"),
        (
            "폴더명 불일치",
            "---\nname: other\n---\n본문\n",
            "[skill-name-mismatch]",
        ),
    ] {
        std::fs::write(skill_dir.join("SKILL.md"), content).unwrap();
        let secret_called = Arc::new(AtomicBool::new(false));
        let flag = secret_called.clone();
        let err = run_with(&settings, &skill_dir, move || {
            flag.store(true, Ordering::SeqCst);
            Err("자격 보관소에 닿으면 안 돼요".to_owned())
        })
        .await
        .expect_err(label);
        assert!(err.contains(want), "{label}: {err}");
        assert!(
            !secret_called.load(Ordering::SeqCst),
            "{label}: SKILL.md 오류에서 자격을 읽었어요"
        );
        assert!(
            !connected.load(Ordering::SeqCst),
            "{label}: SKILL.md 오류에서 원격 서버에 접속했어요"
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
    assert!(err.contains("쓰지 못했어요"), "{err}");
    assert_eq!(std::fs::read_to_string(&pre).unwrap(), "precious");
    assert!(!target.exists());
    // 정상 경로에서는 무작위 접미사로 원자적으로 기록한다
    write_atomic(&target, "ok-lock").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "ok-lock");
}
