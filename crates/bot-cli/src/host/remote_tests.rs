//! 호스트 설정의 원격(Streamable HTTP) 서버 항목: 프로필 등록부에 어떻게 드는가.
use super::*;

const REMOTE: &str = r#"{
    "version": 1,
    "servers": {
        "demo-agenda": {"root": "/opt/bot/servers/demo-agenda", "command": ["bin/server", "--stdio"]},
        "docs": {"transport": "http", "url": "https://mcp.example:8443/api",
                 "auth": {"kind": "bearer", "secret": "docs-token"}}
    }
}"#;

#[test]
fn a_remote_server_is_registered_beside_the_local_ones_with_its_exact_target() {
    let host = parse(REMOTE).unwrap();
    assert!(host.catalog.servers.contains("docs") && host.catalog.servers.contains("demo-agenda"));
    assert_eq!(
        host.catalog.remote_servers,
        BTreeMap::from([("docs".to_owned(), "mcp.example:8443".to_owned())])
    );
    assert_eq!(
        host.remote_servers["docs"]
            .bearer
            .as_ref()
            .map(|name| name.as_str()),
        Some("docs-token")
    );
    // 설치할 파일이 있는 서버(로컬)와 갈라서 담는다
    assert!(host.servers.contains_key("demo-agenda") && !host.servers.contains_key("docs"));
    assert!(!host.catalog.remote_servers.contains_key("demo-agenda"));
}

#[test]
fn a_bad_remote_entry_fails_the_whole_file() {
    for entry in [
        r#"{"transport": "http", "url": "http://mcp.example/api"}"#,
        r#"{"transport": "http", "url": "https://user:pw@mcp.example/api"}"#,
        r#"{"transport": "http", "url": "https://mcp.example/api", "extra": 1}"#,
    ] {
        let text = format!(r#"{{"version": 1, "servers": {{"docs": {entry}}}}}"#);
        let error = parse(&text).err().expect("거절해야 해요").to_string();
        assert!(
            error.contains("servers.docs"),
            "어느 항목인지 알려야 해요: {error}"
        );
    }
}

#[test]
fn the_demo_host_has_no_remote_servers() {
    let host = demo(501);
    assert!(host.remote_servers.is_empty() && host.catalog.remote_servers.is_empty());
}
