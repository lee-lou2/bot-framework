//! Tests for remote Streamable HTTP server entries in host `servers` config.

use super::*;

fn one(entry: Value) -> Result<Servers, String> {
    parse_all(&json!({"docs": entry}))
}

fn bearer(secret: &str) -> Value {
    json!({"kind": "bearer", "secret": secret})
}

fn revision(entry: Value) -> Revision {
    let servers = one(entry).unwrap();
    measure_all(&servers.local, &servers.remote)
        .unwrap()
        .remove("docs")
        .unwrap()
}

#[test]
fn a_remote_entry_is_read_with_an_optional_bearer_beside_the_local_ones() {
    let servers = one(
        json!({"transport": "http", "url": "https://mcp.example:8443/api",
                             "auth": bearer("docs-token")}),
    )
    .unwrap();
    assert!(servers.local.is_empty());
    let docs = &servers.remote["docs"];
    assert_eq!(docs.target_text(), "mcp.example:8443");
    assert_eq!(
        docs.bearer.as_ref().map(SecretName::as_str),
        Some("docs-token")
    );
    let open = one(json!({"transport": "http", "url": "https://mcp.example/api"})).unwrap();
    assert!(open.remote["docs"].bearer.is_none());
    assert_eq!(open.remote["docs"].target_text(), "mcp.example:443");
    // Plaintext HTTP is allowed only for loopback IP literals.
    assert!(one(json!({"transport": "http", "url": "http://127.0.0.1:9000/mcp"})).is_ok());
    let mixed = parse_all(&json!({
        "agenda": {"root": "/opt/bot/servers/agenda", "command": ["bin/server"]},
        "legacy": {"transport": "stdio", "root": "/opt/bot/servers/legacy", "command": ["bin/server"]},
        "docs": {"transport": "http", "url": "https://mcp.example/api"}
    }))
    .unwrap();
    assert_eq!(mixed.local.keys().collect::<Vec<_>>(), ["agenda", "legacy"]);
    assert_eq!(mixed.remote.keys().collect::<Vec<_>>(), ["docs"]);
}

#[test]
fn an_unsafe_address_or_a_bad_shape_is_refused() {
    for url in [
        "http://mcp.example/api",
        "http://10.0.0.5:9000/mcp",
        "http://localhost:9000/mcp",
        "ftp://mcp.example/api",
        "https://user:pass@mcp.example/api",
        "https://user@mcp.example/api",
        "https://mcp.example/api#frag",
        "https://mcp.example:0/api",
        "https://mcp.example:70000/api",
        "https://",
        "mcp.example/api",
        "",
    ] {
        let result = one(json!({"transport": "http", "url": url}));
        assert!(result.is_err(), "{url}");
    }
    let url = "https://mcp.example";
    for entry in [
        json!({"transport": "http"}),
        json!({"transport": "http", "url": 5}),
        json!({"transport": "http", "url": url, "root": "/x"}),
        json!({"transport": "http", "url": url, "headers": {"A": "b"}}),
        json!({"transport": "http", "url": url, "auth": "token"}),
        json!({"transport": "http", "url": url, "auth": {"kind": "basic", "secret": "x"}}),
        json!({"transport": "http", "url": url, "auth": {"kind": "bearer"}}),
        json!({"transport": "http", "url": url, "auth": bearer("Bad Name")}),
        json!({"transport": "http", "url": url, "auth": {"kind": "bearer", "secret": "x", "header": "X-Key"}}),
        json!({"transport": "http", "url": url, "auth": bearer(bot_broker::private::FINGERPRINT_SECRET)}),
    ] {
        assert!(one(entry.clone()).is_err(), "{entry}");
    }
    let alias = parse_all(&json!({"Bad_Alias": {"transport": "http", "url": url}}));
    assert!(alias.is_err());
}

#[test]
fn the_revision_binds_the_address_and_the_bearer_name() {
    let base = json!({"transport": "http", "url": "https://mcp.example/api",
                      "auth": bearer("docs-token")});
    let first = revision(base.clone());
    assert_eq!(first.0.len(), 64);
    assert_eq!(revision(base.clone()), first);
    let explicit = json!({"transport": "http", "url": "https://mcp.example:443/api",
                          "auth": bearer("docs-token")});
    assert_eq!(revision(explicit), first);
    for (label, entry) in [
        (
            "path",
            json!({"transport": "http", "url": "https://mcp.example/other", "auth": bearer("docs-token")}),
        ),
        (
            "port",
            json!({"transport": "http", "url": "https://mcp.example:8443/api", "auth": bearer("docs-token")}),
        ),
        (
            "host",
            json!({"transport": "http", "url": "https://mcp2.example/api", "auth": bearer("docs-token")}),
        ),
        (
            "secret name",
            json!({"transport": "http", "url": "https://mcp.example/api", "auth": bearer("other-token")}),
        ),
        (
            "no auth",
            json!({"transport": "http", "url": "https://mcp.example/api"}),
        ),
    ] {
        assert_ne!(revision(entry), first, "{label}");
    }
}
