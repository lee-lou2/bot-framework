//! Pure `container run` argument assembly, security flag, relay pinning, and validation tests.

use super::*;

#[test]
fn every_variable_that_carries_the_session_token_is_treated_as_a_secret() {
    for name in ["ANTHROPIC_API_KEY", "RELAY_TOKEN"] {
        assert!(crate::model::container::args::secret_like(name), "{name}");
    }
    assert!(!crate::model::container::args::secret_like("HOME"));
}

#[test]
fn the_command_line_is_locked_down_and_the_secret_stays_out_of_it() {
    let fake = Fake::new("args");
    let spec = fake.spec();
    let command = build(&fake.config(), &spec, &plan(Some(relay()))).unwrap();
    let a = &command.args;
    let at = |flag: &str| {
        a.iter()
            .position(|x| x == flag)
            .unwrap_or_else(|| panic!("{flag}: {a:?}"))
    };
    assert_eq!(a[0], "run");
    for flag in ["--rm", "--read-only", "--init", "--no-dns"] {
        at(flag);
    }
    assert_eq!(
        (
            a[at("--tmpfs") + 1].as_str(),
            a[at("--cap-drop") + 1].as_str()
        ),
        ("/tmp", "ALL")
    );
    assert!(
        !a.contains(&"--cap-add".to_owned()),
        "unfirewalled config must not add capabilities"
    );
    assert_eq!(a[at("--network") + 1], "botvm-net");
    assert_eq!(
        (a[at("-c") + 1].as_str(), a[at("-m") + 1].as_str()),
        ("2", "2048M")
    );
    assert_eq!(a[at("-w") + 1], "/bot");
    let owner = args::owner_of(&spec);
    assert_eq!(owner, "personal.run-1.g2.c3");
    for label in [
        format!("{OWNER_LABEL}={}", label_value(&owner)),
        format!("{MANAGED_LABEL}=1"),
        format!("{INSTANCE_LABEL}=botvm"),
        format!("{INCARNATION_LABEL}=inc1"),
    ] {
        assert!(a.contains(&label), "{label}: {a:?}");
    }
    assert_eq!(command.name, a[at("--name") + 1]);
    assert!(is_ours("botvm", &command.name) && command.name.ends_with("-7"));
    assert_eq!(command.name, name_of("botvm", &owner, "inc1", 7));
    at("-i");
    let mounts: Vec<&String> = a
        .iter()
        .zip(a.iter().skip(1))
        .filter(|(f, _)| *f == "--mount")
        .map(|(_, v)| v)
        .collect();
    assert_eq!(mounts.len(), 2);
    assert!(mounts[0].ends_with("target=/bot,readonly"), "{mounts:?}");
    assert!(
        mounts[1].ends_with("target=/work") && !mounts[1].contains("readonly"),
        "{mounts:?}"
    );
    assert!(a.windows(2).any(|w| w == ["-e", "ANTHROPIC_AUTH_TOKEN"]));
    assert!(a.windows(2).any(|w| w == ["-e", "HOME=/work/home"]));
    for pair in [
        "BOT_RELAY_IP=192.168.99.1",
        "BOT_RELAY_PORT=8787",
        "ANTHROPIC_BASE_URL=http://192.168.99.1:8787",
    ] {
        assert_eq!(
            a.windows(2).filter(|w| w == &["-e", pair]).count(),
            1,
            "{pair}: {a:?}"
        );
    }
    assert!(
        !a.iter().any(|x| x.contains("sess-token-secret-1")),
        "{a:?}"
    );
    assert_eq!(
        command.secret_env,
        [(
            "ANTHROPIC_AUTH_TOKEN".to_owned(),
            "sess-token-secret-1".to_owned()
        )]
    );
    let tail: Vec<&str> = a[a.len() - 5..].iter().map(String::as_str).collect();
    assert_eq!(tail, [PINNED, "claude", "-p", "--model", "m"]);
    assert_eq!(ContainerSandbox::owner_of(&spec.attempt), owner);
}

#[test]
fn with_the_trusted_entry_the_model_gets_only_the_minimum_caps_and_the_anchor_a_fixed_non_root_wait()
 {
    let fake = Fake::new("entry");
    let config = fake.firewalled();
    let model = build(&config, &fake.spec(), &plan(Some(relay())))
        .unwrap()
        .args;
    let at = |flag: &str| model.iter().position(|x| x == flag).unwrap();
    assert!(at("--cap-drop") < at("--cap-add"));
    let caps: Vec<&str> = model
        .windows(2)
        .filter(|w| w[0] == "--cap-add")
        .map(|w| w[1].as_str())
        .collect();
    assert_eq!(caps, ["NET_ADMIN", "SETPCAP"]);
    assert!(!model.contains(&"--entrypoint".to_owned()) && !model.contains(&"--user".to_owned()));
    let tail: Vec<&str> = model[model.len() - 5..]
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(tail, [PINNED, "claude", "-p", "--model", "m"]);
    for other in ["bash", "/usr/local/bin/claude", "sh"] {
        let mut spec = fake.spec();
        spec.program = other.into();
        assert_eq!(
            build(&config, &spec, &plan(Some(relay()))).unwrap_err(),
            Refused("program"),
            "{other}"
        );
    }
    let anchor = build_anchor(&config, &plan(None)).unwrap().args;
    let at = |flag: &str| anchor.iter().position(|x| x == flag).unwrap();
    assert_eq!(anchor[at("--entrypoint") + 1], "/usr/local/bin/bot-wait");
    assert_eq!(anchor[at("--user") + 1], "10002:10002");
    assert_eq!(anchor[at("--cap-drop") + 1], "ALL");
    for flag in ["--cap-add", "-e", "--mount", "-i"] {
        assert!(!anchor.contains(&flag.to_owned()), "{flag}: {anchor:?}");
    }
    assert_eq!(anchor.last().unwrap(), PINNED);
    let label = format!("{OWNER_LABEL}={}", label_value(&anchor_owner(&config)));
    assert!(anchor.contains(&label), "{anchor:?}");
    let plain = build_anchor(&fake.config(), &plan(None)).unwrap().args;
    assert_eq!(plain[plain.len() - 3..], [PINNED, "sleep", "2147483647"]);
}

#[test]
fn the_relay_address_is_fixed_by_the_host_and_the_spec_cannot_change_it() {
    let fake = Fake::new("relay");
    let with = |env: Vec<(&str, &str)>| {
        let mut spec = fake.spec();
        spec.env = env
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        build(&fake.config(), &spec, &plan(Some(relay())))
    };
    for env in [
        vec![("ANTHROPIC_BASE_URL", "http://192.168.99.1:8787")],
        vec![("ANTHROPIC_BASE_URL", "http://192.168.99.1:8787/")],
        vec![],
    ] {
        let args = with(env).unwrap().args;
        let urls: Vec<&String> = args
            .iter()
            .filter(|a| a.starts_with("ANTHROPIC_BASE_URL="))
            .collect();
        assert_eq!(urls, ["ANTHROPIC_BASE_URL=http://192.168.99.1:8787"]);
    }
    for env in [
        vec![("ANTHROPIC_BASE_URL", "http://192.168.99.1:9999")],
        vec![("ANTHROPIC_BASE_URL", "http://10.0.0.1:8787")],
        vec![("ANTHROPIC_BASE_URL", "https://api.anthropic.com")],
        vec![("BOT_RELAY_IP", "10.0.0.1")],
        vec![("BOT_RELAY_PORT", "22")],
    ] {
        assert_eq!(with(env.clone()).unwrap_err(), Refused("relay"), "{env:?}");
    }
    for bad in [
        None,
        Some("192.168.99.1:0".parse().unwrap()),
        Some("[::1]:8787".parse().unwrap()),
    ] {
        assert_eq!(
            build(&fake.config(), &fake.spec(), &plan(bad)).unwrap_err(),
            Refused("relay"),
            "{bad:?}"
        );
    }
}

#[test]
fn unsafe_specs_and_configs_are_refused_before_any_command_runs() {
    let fake = Fake::new("refuse");
    let check = |edit: &dyn Fn(&mut ModelSpec, &mut ContainerConfig), want: &'static str| {
        let (mut spec, mut config) = (fake.spec(), fake.config());
        edit(&mut spec, &mut config);
        assert_eq!(
            build(&config, &spec, &plan(Some(relay()))).unwrap_err(),
            Refused(want),
            "{want}"
        );
    };
    let outside =
        std::env::temp_dir().join(format!("bot-container-outside-{}", std::process::id()));
    std::fs::create_dir_all(&outside).unwrap();
    check(
        &|s, _| s.readonly_mounts[0].host = outside.clone(),
        "mount_outside_root",
    );
    check(
        &|s, _| s.readonly_mounts[0].host = fake.root.join("missing"),
        "mount_missing",
    );
    let comma = fake.root.join("a,b");
    std::fs::create_dir_all(&comma).unwrap();
    check(
        &|s, _| s.readonly_mounts[0].host = comma.clone(),
        "mount_path",
    );
    let link = fake.root.join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    check(
        &|s, _| s.readonly_mounts[0].host = link.clone(),
        "mount_outside_root",
    );
    check(&|s, _| s.cwd = "bot".into(), "cwd");
    check(&|s, _| s.cwd = "/bot/../etc".into(), "cwd");
    check(
        &|s, _| s.readonly_mounts[0].guest = "/work".into(),
        "mount_target",
    );
    check(
        &|s, _| s.readonly_mounts[0].guest = "/".into(),
        "mount_target",
    );
    check(&|s, _| s.env.push(("BAD NAME".into(), "x".into())), "env");
    check(&|s, _| s.env.push(("A=B".into(), "x".into())), "env");
    check(&|s, _| s.env.push(("HOME".into(), "dup".into())), "env");
    check(&|s, _| s.env.push(("OK".into(), "a\0b".into())), "env");
    check(&|s, _| s.program = "-rm".into(), "program");
    check(&|s, _| s.args.push("a\0b".into()), "args");
    check(&|_, c| c.image = "--privileged".into(), "image");
    check(&|_, c| c.image = "alpine 3".into(), "image");
    check(&|_, c| c.network = "net;rm".into(), "config");
    check(&|_, c| c.instance = "a/b".into(), "config");
    for entry in [
        Entry {
            anchor: "bot-wait".into(),
            anchor_user: (10002, 10002),
            model_caps: vec![],
        },
        Entry {
            anchor: "/bin/wait".into(),
            anchor_user: (0, 10002),
            model_caps: vec![],
        },
        Entry {
            anchor: "/bin/wait".into(),
            anchor_user: (10002, 0),
            model_caps: vec![],
        },
        Entry {
            anchor: "/bin/wait".into(),
            anchor_user: (1, 1),
            model_caps: vec!["ALL".into()],
        },
        Entry {
            anchor: "/bin/wait".into(),
            anchor_user: (1, 1),
            model_caps: vec!["net_admin".into()],
        },
    ] {
        check(&|_, c| c.entry = Some(entry.clone()), "entry");
    }
    let mut spec = fake.spec();
    spec.env.push(("FLAGLIKE".into(), "--privileged".into()));
    let ok = build(&fake.config(), &spec, &plan(Some(relay()))).unwrap();
    assert!(
        ok.args
            .windows(2)
            .any(|w| w == ["-e", "FLAGLIKE=--privileged"])
    );
}

#[test]
fn only_exact_names_are_ours_and_images_are_pinned_by_digest() {
    for (prefix, id, ours) in [
        ("botvm", "botvm-0123456789ab-3", true),
        ("botvm", "botvm-secondary-0123456789ab-3", false),
        ("botvm", "botvm-0123456789ab", false),
        ("botvm", "botvm-0123456789ab-x", false),
        ("botvm", "botvm-0123456789ab-3-extra", false),
        ("botvm", "botvm-0123456789ab-3-4", false),
        ("botvm", "botvm-0123456789AB-3", false),
        ("botvm", "botvm-0123456789a-3", false),
        ("botvm", "botvm2-0123456789ab-3", false),
        ("botvm-secondary", "botvm-secondary-0123456789ab-3", true),
        ("botvm-secondary", "botvm-0123456789ab-3", false),
        ("botvm", "xbotvm-0123456789ab-3", false),
    ] {
        assert_eq!(is_ours(prefix, id), ours, "{prefix} / {id}");
    }
    let d = DIGEST;
    for (image, want) in [
        ("alpine:3.20", Some(format!("alpine@{d}"))),
        ("alpine", Some(format!("alpine@{d}"))),
        (
            "127.0.0.1:5088/bot-model-vm:latest",
            Some(format!("127.0.0.1:5088/bot-model-vm@{d}")),
        ),
        (
            "127.0.0.1:5088/bot-model-vm",
            Some(format!("127.0.0.1:5088/bot-model-vm@{d}")),
        ),
        (&format!("alpine@{d}"), Some(format!("alpine@{d}"))),
        (
            "alpine@sha256:0000000000000000000000000000000000000000000000000000000000000000",
            None,
        ),
    ] {
        assert_eq!(pin(image, d), want, "{image}");
    }
    for bad in ["sha256:abc", "md5:00", "sha256:ZZ", ""] {
        assert_eq!(pin("alpine:3.20", bad), None, "{bad}");
    }
}

#[test]
fn the_image_entry_preset_matches_the_published_contract_and_builds_both_vms() {
    let entry = Entry::model_image();
    assert_eq!(entry.anchor, "/usr/local/bin/bot-anchor");
    assert_eq!(entry.anchor_user, (10001, 10001));
    assert_eq!(
        entry.model_caps,
        ["NET_ADMIN", "SETGID", "SETPCAP", "SETUID"]
    );
    let fake = Fake::new("preset");
    let mut config = fake.config();
    config.unfirewalled = false;
    config.entry = Some(entry);
    assert!(build(&config, &fake.spec(), &plan(Some(relay()))).is_ok());
    assert!(build_anchor(&config, &plan(None)).is_ok());
}

#[test]
fn firewalled_entry_allows_codex_and_agy_and_pins_their_relay_targets() {
    let fake = Fake::new("multi-cli");
    let config = fake.firewalled();

    // AGY: GOOGLE_GEMINI_BASE_URL pinned by host, arbitrary base URLs refused, GEMINI_API_KEY kept secret.
    let mut agy = fake.spec();
    agy.program = "agy".into();
    agy.args = vec!["--print=".into()];
    agy.env = vec![
        ("HOME".into(), "/work/home".into()),
        (
            "GOOGLE_GEMINI_BASE_URL".into(),
            "http://192.168.99.1:8787".into(),
        ),
        ("GEMINI_API_KEY".into(), "sess-token-agy".into()),
        ("RELAY_TOKEN".into(), "sess-token-agy".into()),
    ];
    let cmd = build(&config, &agy, &plan(Some(relay()))).unwrap();
    assert!(
        cmd.args
            .windows(2)
            .any(|w| w == ["-e", "GOOGLE_GEMINI_BASE_URL=http://192.168.99.1:8787"])
    );
    assert!(
        !cmd.args
            .iter()
            .any(|a| a.starts_with("ANTHROPIC_BASE_URL="))
    );
    assert!(cmd.args.iter().all(|a| !a.contains("sess-token-agy")));
    assert_eq!(
        cmd.secret_env
            .iter()
            .map(|(k, _)| k.as_str())
            .collect::<Vec<_>>(),
        ["GEMINI_API_KEY", "RELAY_TOKEN"]
    );
    let mut bad_agy = agy.clone();
    bad_agy.env[1].1 = "https://generativelanguage.googleapis.com".into();
    assert_eq!(
        build(&config, &bad_agy, &plan(Some(relay()))).unwrap_err(),
        Refused("relay")
    );

    // Codex: -c relay URLs must match the host-pinned relay; neither Anthropic nor Gemini base URL env is injected.
    let mut codex = fake.spec();
    codex.program = "codex".into();
    codex.args = vec![
        "exec".into(),
        "-c".into(),
        "model_provider=\"relay\"".into(),
        "-c".into(),
        "model_providers.relay.base_url=\"http://192.168.99.1:8787/v1\"".into(),
        "-c".into(),
        "mcp_servers.relay.url=\"http://192.168.99.1:8787/mcp\"".into(),
        "-".into(),
    ];
    codex.env = vec![
        ("HOME".into(), "/work/home".into()),
        ("RELAY_TOKEN".into(), "sess-token-codex".into()),
    ];
    let cmd = build(&config, &codex, &plan(Some(relay()))).unwrap();
    assert!(!cmd.args.iter().any(|a| {
        a.starts_with("ANTHROPIC_BASE_URL=") || a.starts_with("GOOGLE_GEMINI_BASE_URL=")
    }));
    for bad_arg in [
        "model_provider=\"openai\"",
        "model_providers.relay.base_url=\"https://api.openai.com/v1\"",
        "mcp_servers.relay.url=\"http://10.0.0.9:8787/mcp\"",
        "model_providers.relay.base_url = \"http://192.168.99.1:8787/v1\"",
        "model_providers.relay.base_url= \"http://192.168.99.1:8787/v1\"",
        "model_providers={relay={base_url=\"https://evil.example/v1\"}}",
        "model_providers.relay={base_url=\"https://evil.example/v1\"}",
        "model_providers.other.base_url=\"https://evil.example/v1\"",
        "mcp_servers={relay={url=\"https://evil.example/mcp\"}}",
        "mcp_servers.relay={url=\"https://evil.example/mcp\"}",
        "mcp_servers.other.url=\"https://evil.example/mcp\"",
        "mcp_servers.relay.command=\"sh\"",
        "\"model_providers\".relay.base_url=\"https://evil.example/v1\"",
    ] {
        let mut bad_codex = codex.clone();
        bad_codex.args.push("-c".into());
        bad_codex.args.push(bad_arg.into());
        assert_eq!(
            build(&config, &bad_codex, &plan(Some(relay()))).unwrap_err(),
            Refused("relay"),
            "{bad_arg}"
        );
    }
    // Missing any required relay override must fail closed.
    for missing in [
        vec!["exec".into(), "-".into()],
        vec![
            "exec".into(),
            "-c".into(),
            "model_providers.relay.base_url=\"http://192.168.99.1:8787/v1\"".into(),
            "-c".into(),
            "mcp_servers.relay.url=\"http://192.168.99.1:8787/mcp\"".into(),
            "-".into(),
        ],
        vec![
            "exec".into(),
            "-c".into(),
            "model_provider=\"relay\"".into(),
            "-c".into(),
            "model_providers.relay.base_url=\"http://192.168.99.1:8787/v1\"".into(),
            "-".into(),
        ],
        vec![
            "exec".into(),
            "-c".into(),
            "model_provider=\"relay\"".into(),
            "-c".into(),
            "mcp_servers.relay.url=\"http://192.168.99.1:8787/mcp\"".into(),
            "-".into(),
        ],
    ] {
        let mut incomplete = codex.clone();
        incomplete.args = missing.clone();
        assert_eq!(
            build(&config, &incomplete, &plan(Some(relay()))).unwrap_err(),
            Refused("relay"),
            "{missing:?}"
        );
    }

    // Invalid per-program image entries fail config validation.
    let bad_img = fake.config().with_image_for("codex", "--privileged");
    assert_eq!(
        build(&bad_img, &fake.spec(), &plan(Some(relay()))).unwrap_err(),
        Refused("image")
    );
}
