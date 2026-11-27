//! Process-level startup and activation tests: failed starts preserve the operating revision, identity/egress changes trigger activation, and weak HTTP keys or oversized memory seeds fail preflight.
use crate::process_support::*;
use std::path::{Path, PathBuf};

#[test]
fn a_start_that_fails_before_opening_never_changes_the_operating_revision() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let profiles = examples();
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let url = schema.url();
    let server = start_gateway(&refs(&env_for(&profiles, &url, &socket, &[])), &[], &socket);
    wait_for("first start did not publish an active revision", || {
        !schema.active().is_empty()
    });
    let operating = schema.active();

    // Activating revision B while A holds the socket must fail without altering the active revision.
    let changed = dir.path().join("profiles");
    copy_dir(&profiles, &changed);
    std::fs::write(
        changed.join("personal/agents/main/AGENT.md"),
        "Different instructions",
    )
    .unwrap();
    let env = env_for(&changed, &url, &socket, &[]);
    let busy = bot(&refs(&env), &["serve", "--role", "gateway", "--activate"]);
    assert_eq!(busy.status.code(), Some(1), "{}", text(&busy));
    assert!(
        text(&busy).contains("already using this socket"),
        "{}",
        text(&busy)
    );
    assert_eq!(
        schema.active(),
        operating,
        "failed start changed the active revision"
    );

    // Missing entrance secret also fails before publication.
    let keyless = dir.path().join("keyless");
    copy_dir(&changed, &keyless);
    let profile_yaml = keyless.join("personal/profile.yaml");
    let yaml = std::fs::read_to_string(&profile_yaml)
        .unwrap()
        .replace(
            "  - {id: desk, adapter: cli, account: owner, agent: main}",
            "  - {id: desk, adapter: cli, account: owner, agent: main}\n  - {id: inbox, adapter: http, endpoint: inbox, agent: main}",
        )
        .replace(
            "  - {identity: \"local:owner\", roles: [owner]}",
            "  - {identity: \"local:owner\", roles: [owner]}\n  - {identity: \"http:inbox/svc\", roles: [reader]}",
        );
    std::fs::write(&profile_yaml, yaml).unwrap();
    let host = dir.path().join("host.json");
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            r#", "http": {"listen": "127.0.0.1:0",
                "endpoints": {"inbox": {"issuer": "i", "audience": "a", "key": "inbox-key"}}}"#,
            "",
        ),
    )
    .unwrap();
    let env = env_for(
        &keyless,
        &url,
        &socket,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    let missing = bot(&refs(&env), &["serve", "--role", "gateway", "--activate"]);
    assert!(!missing.status.success(), "{}", text(&missing));
    let message = text(&missing);
    assert!(
        message.contains("inbox-key") && message.contains("BOT_SECRET__personal__inbox_key"),
        "{message}"
    );
    assert_eq!(
        schema.active(),
        operating,
        "start failing on missing secret changed the active revision"
    );

    // Once A stops, B activates cleanly and updates the active revision.
    drop(server);
    let env = env_for(&changed, &url, &socket, &[]);
    let server = start_gateway(&refs(&env), &["--activate"], &socket);
    wait_for("successful start did not publish the new revision", || {
        schema.active() != operating
    });
    drop(server);
}

fn write_host(dir: &Path, name: &str, uid: u32) -> PathBuf {
    let pkg = package(dir, "#!/bin/sh\nexit 0\n");
    let path = dir.join(name);
    std::fs::write(&path, host_json(uid, &pkg, "", "")).unwrap();
    path
}

#[test]
fn a_changed_identity_mapping_is_an_activation_event_that_closes_the_old_gateway() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let profiles = examples();
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let (socket_a, socket_b) = (
        dir.path().join("a").join("g.sock"),
        dir.path().join("b").join("g.sock"),
    );
    let url = schema.url();
    let host_a = write_host(dir.path(), "host-a.json", my_uid());
    let host_b = write_host(dir.path(), "host-b.json", my_uid() + 1);
    let env_a = env_for(
        &profiles,
        &url,
        &socket_a,
        &[("BOT_HOST_CONFIG", host_a.to_str().unwrap())],
    );
    let env_b = env_for(
        &profiles,
        &url,
        &socket_b,
        &[("BOT_HOST_CONFIG", host_b.to_str().unwrap())],
    );
    let a = start_gateway(&refs(&env_a), &[], &socket_a);
    wait_for("first start did not publish an active revision", || {
        !schema.active().is_empty()
    });
    let operating = schema.active();
    // Joining with the same mapping writes neither publication audits nor new memory seeds.
    let publication_writes = || {
        schema.query(
            "select count(*) from {s}.bot_audit where action in ('active.replace', 'memory.seed')",
        )
    };
    let (audit, memories) = (publication_writes(), schema.count("bot_memories"));
    let socket_c = dir.path().join("c").join("g.sock");
    let env_c = env_for(
        &profiles,
        &url,
        &socket_c,
        &[("BOT_HOST_CONFIG", host_a.to_str().unwrap())],
    );
    let joined = start_gateway(&refs(&env_c), &[], &socket_c);
    let hello = bot(
        &refs(&env_c),
        &["chat", "--no-wait", "--request-id", "join-1", "hello"],
    );
    assert!(hello.status.success(), "{}", text(&hello));
    assert_eq!(schema.active(), operating);
    assert_eq!(
        (publication_writes(), schema.count("bot_memories")),
        (audit.clone(), memories),
        "join must not write publication audit or memory seeds"
    );
    assert_ne!(audit, "0", "initial publish must record audit");
    drop(joined);

    // Different UID mapping requires `--activate`; activating fences the old gateway (exit code 3).
    let refused = bot(&refs(&env_b), &["serve", "--role", "gateway"]);
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(text(&refused).contains("--activate"), "{}", text(&refused));
    assert_eq!(schema.active(), operating);
    let b = start_gateway(&refs(&env_b), &["--activate"], &socket_b);
    wait_for("mapping change did not update the active revision", || {
        schema.active() != operating
    });
    let runs = schema.count("bot_runs");
    let stale = bot(
        &refs(&env_a),
        &["chat", "--no-wait", "--request-id", "stale-1", "hello"],
    );
    assert_eq!(stale.status.code(), Some(3), "{}", text(&stale));
    assert_eq!(
        schema.count("bot_runs"),
        runs,
        "stale gateway must not accept runs"
    );
    drop((a, b));
}

#[test]
fn a_changed_model_allowance_or_egress_cap_is_an_activation_event() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let profiles = examples();
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let write = |name: &str, extra: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, host_json(my_uid(), &pkg, "", extra)).unwrap();
        path
    };
    let base = write("base.json", "");
    let wider = write(
        "wider.json",
        r#", "egress": {"personal": ["calendar.example:443"]}"#,
    );
    let url = schema.url();
    let socket = |name: &str| dir.path().join(name).join("g.sock");
    let env = |host: &Path, name: &str| {
        env_for(
            &profiles,
            &url,
            &socket(name),
            &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
        )
    };
    let server = start_gateway(&refs(&env(&base, "a")), &[], &socket("a"));
    wait_for("first start did not publish an active revision", || {
        !schema.active().is_empty()
    });
    let operating = schema.active();
    let refused = bot(&refs(&env(&wider, "b")), &["serve", "--role", "gateway"]);
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(text(&refused).contains("--activate"), "{}", text(&refused));
    assert_eq!(schema.active(), operating);
    let shown = |host: &Path| {
        let out = bot(&refs(&env(host, "v")), &["validate"]);
        assert!(out.status.success(), "{}", text(&out));
        text(&out)
            .lines()
            .find(|line| {
                line.to_ascii_lowercase()
                    .starts_with("activation revision ")
            })
            .map(str::to_owned)
            .unwrap()
    };
    assert_ne!(shown(&base), shown(&wider));
    drop(server);
}

fn with_http_entrance(from: &Path, to: &Path) {
    copy_dir(from, to);
    let yaml_path = to.join("personal/profile.yaml");
    let yaml = std::fs::read_to_string(&yaml_path)
        .unwrap()
        .replace(
            "  - {id: desk, adapter: cli, account: owner, agent: main}",
            "  - {id: desk, adapter: cli, account: owner, agent: main}\n  - {id: inbox, adapter: http, endpoint: inbox, agent: main}",
        )
        .replace(
            "  - {identity: \"local:owner\", roles: [owner]}",
            "  - {identity: \"local:owner\", roles: [owner]}\n  - {identity: \"http:inbox/svc\", roles: [reader]}",
        );
    std::fs::write(yaml_path, yaml).unwrap();
}

#[test]
fn an_http_key_shorter_than_32_bytes_stops_the_start_and_the_validation() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let profiles = dir.path().join("profiles");
    with_http_entrance(&examples(), &profiles);
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            r#", "http": {"listen": "127.0.0.1:0",
                "endpoints": {"inbox": {"issuer": "i", "audience": "a", "key": "inbox-key"}}}"#,
            "",
        ),
    )
    .unwrap();
    let url = schema.url();
    let socket = dir.path().join("run").join("g.sock");
    let short = "k".repeat(31);
    let env = env_for(
        &profiles,
        &url,
        &socket,
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_SECRET__personal__inbox_key", short.as_str()),
        ],
    );
    let validate = bot(&refs(&env), &["validate"]);
    assert!(!validate.status.success(), "{}", text(&validate));
    assert!(
        text(&validate).to_ascii_lowercase().contains("too short"),
        "{}",
        text(&validate)
    );
    assert!(
        !text(&validate).contains("Validation passed"),
        "validate reported pass despite error: {}",
        text(&validate)
    );
    let serve = bot(&refs(&env), &["serve", "--role", "gateway", "--activate"]);
    assert!(!serve.status.success(), "{}", text(&serve));
    assert!(
        text(&serve).to_ascii_lowercase().contains("too short"),
        "{}",
        text(&serve)
    );
    assert_eq!(
        schema.active(),
        "",
        "failed start must not publish anything"
    );
    let exact = "k".repeat(32);
    let env = env_for(
        &profiles,
        &url,
        &socket,
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_SECRET__personal__inbox_key", exact.as_str()),
        ],
    );
    let validate = bot(&refs(&env), &["validate"]);
    assert!(validate.status.success(), "{}", text(&validate));
}

#[test]
fn a_memory_seed_over_the_store_limit_stops_the_validation_and_the_start_before_any_write() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL is not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let profiles = dir.path().join("profiles");
    copy_dir(&examples(), &profiles);
    let seed = profiles.join("personal/agents/main/memory/routine.md");
    let body = "€".repeat(2731); // 8,193 UTF-8 bytes (> 8 KiB cap)
    std::fs::write(&seed, format!("---\nowner: \"local:owner\"\n---\n{body}")).unwrap();
    let url = schema.url();
    let socket = dir.path().join("run").join("g.sock");
    let env = env_for(&profiles, &url, &socket, &[]);
    let validate = bot(&refs(&env), &["validate"]);
    assert!(!validate.status.success(), "{}", text(&validate));
    let message = text(&validate);
    assert!(
        message.contains("too long") && message.contains("memory/routine.md"),
        "{message}"
    );
    assert!(
        !message.contains("Validation passed"),
        "validate reported pass despite error: {message}"
    );
    let serve = bot(&refs(&env), &["serve", "--role", "gateway", "--activate"]);
    assert!(!serve.status.success(), "{}", text(&serve));
    assert!(text(&serve).contains("too long"), "{}", text(&serve));
    // Stops before touching the store, so even migrations have not run.
    assert_eq!(schema.tables(), "0", "must not write anything to store");
}
