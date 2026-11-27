use super::*;
use serde_json::json;
use std::collections::BTreeSet;

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn assert_matches_any(message: &str, needles: &[&str], ctx: &Value) {
    let lower = message.to_ascii_lowercase();
    assert!(
        needles.iter().any(|n| lower.contains(n)),
        "{ctx} → {message} (expected one of {needles:?})"
    );
}

#[test]
fn providers_are_demo_or_a_claude_cli_with_an_https_upstream_and_a_secret_name() {
    let ok = parse_providers(&json!({
        "assistant": {"kind": "demo"},
        "claude": {"kind": "claude-cli", "model": "claude-x", "endpoint": "https://api.example.com:8443/v1/messages", "secret": "upstream-key"}
    }))
    .unwrap();
    assert_eq!(ok["assistant"], Provider::Demo);
    assert_eq!(
        ok["claude"],
        Provider::ClaudeCli {
            model: "claude-x".into(),
            endpoint: "https://api.example.com:8443/v1/messages".into(),
            secret: "upstream-key".into(),
            effort: None,
        }
    );
    assert!(parse_providers(&json!({})).unwrap().is_empty());
    let claude = |patch: Value| {
        let mut base = json!({"kind": "claude-cli", "model": "m", "endpoint": "https://h.example/v1", "secret": "k"});
        base.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        json!({"a": base})
    };
    let cases: &[(Value, &[&str])] = &[
        (json!([]), &["object"]),
        (
            json!({"Bad": {"kind": "demo"}}),
            &["name", "alias", "lowercase", "invalid"],
        ),
        (json!({"a": "demo"}), &["object"]),
        (json!({"a": {}}), &["kind"]),
        (
            json!({"a": {"kind": "demo", "model": "x"}}),
            &["unknown", "model"],
        ),
        (json!({"a": {"kind": "pi"}}), &["pi"]),
        (
            json!({"a": {"kind": "openai"}}),
            &["unsupported", "not supported", "openai"],
        ),
        (
            json!({"a": {"kind": "claude-cli", "model": "m", "secret": "k"}}),
            &["endpoint"],
        ),
        (
            claude(json!({"endpoint": "http://h.example/v1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://user:pw@h.example/v1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://h.example/v1?key=1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://h.example/v1#frag"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://h ost/v1"})),
            &["https://"],
        ),
        (claude(json!({"endpoint": "https:///v1"})), &["https://"]),
        (
            claude(json!({"endpoint": "https://h.example:70000/v1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://h.example:0/v1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://h.example:/v1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://h.example:8a/v1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://[::1]:443/v1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://h.example/v1\r\nX-Evil: 1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://h.example/v1\t"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://h.example/%2e%2e/x"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://0x7f000001/v1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://127.0x1/v1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://example.0x1/v1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://2130706433/v1"})),
            &["https://"],
        ),
        (
            claude(json!({"endpoint": "https://0177.0.0.1/v1"})),
            &["https://"],
        ),
        (claude(json!({"secret": "Bad Name"})), &["secret"]),
        (claude(json!({"model": ""})), &["model"]),
        (claude(json!({"extra": 1})), &["unknown", "extra"]),
    ];
    for (value, needles) in cases {
        let message = parse_providers(value).unwrap_err();
        assert_matches_any(&message, needles, value);
    }
}

#[test]
fn sandbox_paths_are_absolute_and_the_vm_image_is_pinned_to_a_digest() {
    let digest = "a".repeat(64);
    let ok = parse_sandbox(&json!({
        "srt": {"binary": "/opt/srt/bin/srt", "work_root": "/var/bot/work"},
        "apple": {"image": format!("registry.example/bot-vm@sha256:{digest}"), "network": "bot-internal", "relay_bind": "192.168.64.1:9300"}
    }))
    .unwrap();
    assert_eq!(
        ok.srt.as_ref().unwrap().binary,
        PathBuf::from("/opt/srt/bin/srt")
    );
    assert!(ok.srt.as_ref().unwrap().extra_path.is_empty());
    let with_extra = parse_sandbox(&json!({
        "srt": {
            "binary": "/opt/srt/bin/srt",
            "work_root": "/var/bot/work",
            "extra_path": ["/opt/homebrew/bin", "/opt/tools/bin", "/opt/homebrew/bin"]
        }
    }))
    .unwrap();
    assert_eq!(
        with_extra.srt.unwrap().extra_path,
        vec![
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/opt/tools/bin")
        ]
    );
    assert_eq!(
        ok.apple.as_ref().unwrap().relay_bind.unwrap().to_string(),
        "192.168.64.1:9300"
    );
    let bare =
        parse_sandbox(&json!({"apple": {"image": format!("img@sha256:{digest}"), "network": "n"}}))
            .unwrap()
            .apple
            .unwrap();
    assert_eq!(
        (bare.relay_bind, bare.work_root, bare.instance, bare.binary),
        (None, None, None, None)
    );
    let full = parse_sandbox(
        &json!({"apple": {"image": format!("img@sha256:{digest}"), "network": "n",
        "relay_bind": "192.168.64.1:0", "work_root": "/var/bot/models", "instance": "worker-a",
        "binary": "/opt/homebrew/bin/container"}}),
    )
    .unwrap()
    .apple
    .unwrap();
    assert_eq!(full.relay_bind.unwrap().port(), 0);
    assert_eq!(full.work_root, Some(PathBuf::from("/var/bot/models")));
    assert_eq!(full.instance.as_deref(), Some("worker-a"));
    assert_eq!(
        full.binary,
        Some(PathBuf::from("/opt/homebrew/bin/container"))
    );
    assert_eq!(parse_sandbox(&json!({})).unwrap(), Sandbox::default());
    let apple = |patch: Value| {
        let mut base = json!({"image": format!("img@sha256:{digest}"), "network": "n", "relay_bind": "192.168.64.1:9300"});
        base.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        json!({"apple": base})
    };
    let cases: &[(Value, &[&str])] = &[
        (json!("x"), &["object"]),
        (json!({"vm": {}}), &["unknown", "vm"]),
        (
            json!({"srt": {"binary": "srt", "work_root": "/w"}}),
            &["absolute"],
        ),
        (
            json!({"srt": {"binary": "/srt", "work_root": "/"}}),
            &["absolute"],
        ),
        (json!({"srt": {"binary": "/srt"}}), &["work_root"]),
        (
            json!({"srt": {"binary": "/srt", "work_root": "/w", "path": "/x"}}),
            &["unknown", "path"],
        ),
        (
            json!({"srt": {"binary": "/srt", "work_root": "/w", "extra_path": "/opt/bin"}}),
            &["array"],
        ),
        (
            json!({"srt": {"binary": "/srt", "work_root": "/w", "extra_path": ["rel/bin"]}}),
            &["absolute"],
        ),
        (
            json!({"srt": {"binary": "/srt", "work_root": "/w", "extra_path": ["/"]}}),
            &["absolute"],
        ),
        (
            json!({"srt": {"binary": "/srt", "work_root": "/w", "extra_path": ["/opt/a:/opt/b"]}}),
            &[":"],
        ),
        (
            json!({"srt": {"binary": "/srt", "work_root": "/w", "extra_path": (0..17).map(|i| format!("/opt/b{i}")).collect::<Vec<_>>()}}),
            &["16"],
        ),
        (
            apple(json!({"image": "registry.example/bot-vm:latest"})),
            &["digest", "sha256"],
        ),
        (
            apple(json!({"image": "img@sha256:abc"})),
            &["digest", "sha256"],
        ),
        (
            apple(json!({"image": format!("img@sha256:{}", "A".repeat(64))})),
            &["digest", "sha256"],
        ),
        (
            apple(json!({"relay_bind": "0.0.0.0:9300"})),
            &["0.0.0.0", "unspecified", "all interfaces", "wildcard"],
        ),
        (
            apple(json!({"relay_bind": "[::]:9300"})),
            &["0.0.0.0", "unspecified", "all interfaces", "wildcard"],
        ),
        (
            apple(json!({"relay_bind": "127.0.0.1:9300"})),
            &["loopback"],
        ),
        (apple(json!({"relay_bind": "[::1]:9300"})), &["loopback"]),
        (apple(json!({"relay_bind": "[fd00::1]:9300"})), &["ipv6"]),
        (apple(json!({"work_root": "models"})), &["absolute"]),
        (apple(json!({"work_root": "/"})), &["absolute"]),
        (apple(json!({"instance": "Bad Name"})), &["kebab-case"]),
        (apple(json!({"instance": ""})), &["instance"]),
        (apple(json!({"binary": "container"})), &["absolute"]),
        (
            apple(json!({"relay_bind": "not-an-address"})),
            &["host:port", "addr:port", "address", "relay_bind"],
        ),
        (apple(json!({"network": ""})), &["network"]),
    ];
    for (value, needles) in cases {
        let message = parse_sandbox(value).unwrap_err();
        assert_matches_any(&message, needles, value);
    }
}

fn catalog() -> HostCatalog {
    HostCatalog {
        servers: set(&["agenda"]),
        models: [("personal".to_owned(), set(&["assistant"]))].into(),
        egress: [("personal".to_owned(), set(&["calendar.example:443"]))].into(),
        ..HostCatalog::default()
    }
}

fn claude(endpoint: &str, secret: &str) -> BTreeMap<String, Provider> {
    [(
        "assistant".to_owned(),
        Provider::ClaudeCli {
            model: "m".into(),
            endpoint: endpoint.into(),
            secret: secret.into(),
            effort: None,
        },
    )]
    .into()
}

fn revisions(value: &str) -> BTreeMap<String, Revision> {
    [("agenda".to_owned(), Revision(value.into()))].into()
}

fn sandbox(image: &str, work: &str, relay: &str) -> Sandbox {
    Sandbox {
        srt: Some(Srt {
            binary: "/srt".into(),
            work_root: work.into(),
            extra_path: Vec::new(),
        }),
        apple: Some(Apple {
            image: image.into(),
            network: "net".into(),
            relay_bind: Some(relay.parse().unwrap()),
            work_root: None,
            instance: None,
            binary: None,
        }),
    }
}

#[test]
fn authority_changes_change_the_fingerprint_but_host_paths_and_addresses_do_not() {
    let gateway = [7u8; 32];
    let base = authority_fingerprint(
        &gateway,
        &catalog(),
        &claude("https://up.example/v1", "key"),
        &sandbox("img@sha256:aa", "/work/a", "192.168.64.1:9300"),
        &revisions("r1"),
    );
    let with = |edit: &dyn Fn(&mut HostCatalog),
                providers: BTreeMap<String, Provider>,
                sandbox: Sandbox,
                revs: BTreeMap<String, Revision>,
                gateway: [u8; 32]| {
        let mut c = catalog();
        edit(&mut c);
        authority_fingerprint(&gateway, &c, &providers, &sandbox, &revs)
    };
    let same = |_: &mut HostCatalog| {};
    let up = || claude("https://up.example/v1", "key");
    let vm = || sandbox("img@sha256:aa", "/work/a", "192.168.64.1:9300");
    assert_eq!(
        base,
        with(&same, up(), vm(), revisions("r1"), gateway),
        "deterministic"
    );
    for (what, changed) in [
        (
            "gateway mapping",
            with(&same, up(), vm(), revisions("r1"), [8; 32]),
        ),
        (
            "allowed models",
            with(
                &|c| {
                    c.models.get_mut("personal").unwrap().insert("other".into());
                },
                up(),
                vm(),
                revisions("r1"),
                gateway,
            ),
        ),
        (
            "allowed model profile",
            with(
                &|c| {
                    c.models.insert("work".into(), set(&["assistant"]));
                },
                up(),
                vm(),
                revisions("r1"),
                gateway,
            ),
        ),
        (
            "egress",
            with(
                &|c| {
                    c.egress
                        .get_mut("personal")
                        .unwrap()
                        .insert("evil.example:443".into());
                },
                up(),
                vm(),
                revisions("r1"),
                gateway,
            ),
        ),
        (
            "server alias",
            with(
                &|c| {
                    c.servers.insert("extra".into());
                },
                up(),
                vm(),
                revisions("r1"),
                gateway,
            ),
        ),
        (
            "installed revision",
            with(&same, up(), vm(), revisions("r2"), gateway),
        ),
        (
            "upstream endpoint",
            with(
                &same,
                claude("https://other.example/v1", "key"),
                vm(),
                revisions("r1"),
                gateway,
            ),
        ),
        (
            "secret name",
            with(
                &same,
                claude("https://up.example/v1", "other-key"),
                vm(),
                revisions("r1"),
                gateway,
            ),
        ),
        (
            "provider kind",
            with(
                &same,
                [("assistant".to_owned(), Provider::Demo)].into(),
                vm(),
                revisions("r1"),
                gateway,
            ),
        ),
        (
            "VM image",
            with(
                &same,
                up(),
                sandbox("img@sha256:bb", "/work/a", "192.168.64.1:9300"),
                revisions("r1"),
                gateway,
            ),
        ),
        (
            "VM network",
            with(
                &same,
                up(),
                Sandbox {
                    apple: Some(Apple {
                        network: "other-net".into(),
                        ..vm().apple.unwrap()
                    }),
                    ..vm()
                },
                revisions("r1"),
                gateway,
            ),
        ),
    ] {
        assert_ne!(changed, base, "{what}");
    }
    // Host-local paths, addresses, and binary locations are deployment details, not authority.
    for (what, same_print) in [
        (
            "work root",
            with(
                &same,
                up(),
                sandbox("img@sha256:aa", "/work/b", "192.168.64.1:9300"),
                revisions("r1"),
                gateway,
            ),
        ),
        (
            "relay bind",
            with(
                &same,
                up(),
                sandbox("img@sha256:aa", "/work/a", "10.0.0.5:9400"),
                revisions("r1"),
                gateway,
            ),
        ),
        (
            "omitted relay bind, VM work root, instance, binary",
            with(
                &same,
                up(),
                Sandbox {
                    apple: Some(Apple {
                        relay_bind: None,
                        work_root: Some("/var/bot/models".into()),
                        instance: Some("worker-b".into()),
                        binary: Some("/usr/local/bin/container".into()),
                        ..vm().apple.unwrap()
                    }),
                    ..vm()
                },
                revisions("r1"),
                gateway,
            ),
        ),
    ] {
        assert_eq!(same_print, base, "{what}");
    }
    // Empty revision map differs from a pinned installed revision.
    assert_ne!(base, with(&same, up(), vm(), BTreeMap::new(), gateway));
}

#[test]
fn field_boundaries_cannot_be_shifted_to_collide() {
    let gateway = [0u8; 32];
    let print = |models: &[(&str, &[&str])]| {
        let catalog = HostCatalog {
            models: models
                .iter()
                .map(|(profile, aliases)| ((*profile).to_owned(), set(aliases)))
                .collect(),
            ..HostCatalog::default()
        };
        authority_fingerprint(
            &gateway,
            &catalog,
            &BTreeMap::new(),
            &Sandbox::default(),
            &BTreeMap::new(),
        )
    };
    assert_ne!(print(&[("ab", &["c"])]), print(&[("a", &["bc"])]));
    assert_ne!(print(&[("a", &["b", "c"])]), print(&[("a", &["bc"])]));
}
