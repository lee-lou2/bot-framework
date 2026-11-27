//! Process tests for admin token file lifecycle: exclusive locking across starts, rollback before active-revision
//! changes on publish failure, and journal recovery after SIGKILL between publish and DB commit.
use crate::process_support::*;
use std::{
    path::Path,
    process::{Command, Stdio},
};

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[test]
fn a_failed_admin_start_never_deletes_the_secret_files_of_a_running_admin_or_an_existing_one() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let host = dir.path().join("host.json");
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            "",
            &format!(r#", "admin": {{"bind": "{addr}"}}"#),
        ),
    )
    .unwrap();
    let url = schema.url();
    let profiles = examples();
    let envs = |socket: &str, token_file: &Path| {
        env_for(
            &profiles,
            &url,
            &dir.path().join(socket).join("g.sock"),
            &[
                ("BOT_HOST_CONFIG", host.to_str().unwrap()),
                ("BOT_ADMIN_TOKEN_FILE", token_file.to_str().unwrap()),
            ],
        )
    };
    let token_a = dir.path().join("a.token");
    let env_a = envs("a", &token_a);
    let server = start_gateway_roles(
        &refs(&env_a),
        "gateway,admin",
        &dir.path().join("a").join("g.sock"),
    );
    wait_for("admin A did not publish its token file", || {
        token_a.exists()
    });
    let secret = std::fs::read_to_string(&token_a).unwrap();
    let ok = |token: &str| http_get(&addr, "/v1/admin/me", Some(token.trim())).0 == 200;
    assert!(ok(&secret));

    // A second start targeting the same token file must fail without modifying A's token.
    let env_b = envs("b", &token_a);
    let second = bot(&refs(&env_b), &["serve", "--role", "admin"]);
    assert!(!second.status.success(), "{}", text(&second));
    let msg = text(&second);
    let low = msg.to_ascii_lowercase();
    assert!(
        low.contains("another process") || low.contains("in use") || low.contains("lock"),
        "{msg}"
    );
    assert_eq!(std::fs::read_to_string(&token_a).unwrap(), secret);
    assert!(ok(&secret), "admin A must remain authenticated");

    // Failing to bind a busy port must leave an existing file at a different token path intact.
    let token_c = dir.path().join("c.token");
    std::fs::write(&token_c, "original-secret\n").unwrap();
    let env_c = envs("c", &token_c);
    let busy = bot(&refs(&env_c), &["serve", "--role", "admin"]);
    assert!(!busy.status.success(), "{}", text(&busy));
    assert_eq!(
        std::fs::read_to_string(&token_c).unwrap(),
        "original-secret\n",
        "failed start must not modify or remove an existing file"
    );
    assert_eq!(std::fs::read_to_string(&token_a).unwrap(), secret);
    assert!(ok(&secret));
    drop(server);
}

#[test]
fn a_failed_secret_publication_stops_the_start_before_the_active_revision_changes() {
    use std::os::unix::fs::PermissionsExt;
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let url = schema.url();
    let profiles = examples();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(my_uid(), &pkg, "", r#", "admin": {"bind": "127.0.0.1:0"}"#),
    )
    .unwrap();
    let socket = dir.path().join("a").join("g.sock");
    let env_a = env_for(
        &profiles,
        &url,
        &socket,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    let server = start_gateway(&refs(&env_a), &[], &socket);
    wait_for("first start did not publish active revision", || {
        !schema.active().is_empty()
    });
    let operating = schema.active();

    // Read-only dir with pre-existing lockfile allows claiming the lock while causing publish to fail.
    let changed = dir.path().join("profiles");
    copy_dir(&profiles, &changed);
    std::fs::write(
        changed.join("personal/agents/main/AGENT.md"),
        "updated instructions",
    )
    .unwrap();
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    let token = locked.join("admin.token");
    std::fs::write(&token, "original-secret\n").unwrap();
    std::fs::write(locked.join("admin.token.lock"), "").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    let env_b = env_for(
        &changed,
        &url,
        &dir.path().join("b").join("g.sock"),
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_ADMIN_TOKEN_FILE", token.to_str().unwrap()),
        ],
    );
    let failed = bot(&refs(&env_b), &["serve", "--role", "admin", "--activate"]);
    assert!(!failed.status.success(), "{}", text(&failed));
    assert!(text(&failed).contains("admin.token"), "{}", text(&failed));
    assert_eq!(
        schema.active(),
        operating,
        "failed token publication must not change active revision"
    );
    assert_eq!(
        std::fs::read_to_string(&token).unwrap(),
        "original-secret\n"
    );
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    drop(server);
}

#[test]
fn a_forced_kill_between_the_secret_and_the_commit_is_repaired_by_the_next_start() {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let url = schema.url();
    let profiles = examples();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let write_host = |name: &str, extra: &str| {
        let path = dir.path().join(name);
        std::fs::write(
            &path,
            host_json(
                my_uid(),
                &pkg,
                "",
                &format!(r#", "admin": {{"bind": "127.0.0.1:0"}}{extra}"#),
            ),
        )
        .unwrap();
        path
    };
    let base = write_host("base.json", "");
    let socket = dir.path().join("a").join("g.sock");
    let env_a = env_for(
        &profiles,
        &url,
        &socket,
        &[("BOT_HOST_CONFIG", base.to_str().unwrap())],
    );
    let server = start_gateway(&refs(&env_a), &[], &socket);
    wait_for("first start did not publish active revision", || {
        !schema.active().is_empty()
    });
    let operating = schema.active();

    // Hold a row lock on `bot_active` so the new admin blocks after publishing the token and before commit.
    let holder = schema.hold_active_row();

    let wider = write_host(
        "wider.json",
        r#", "egress": {"personal": ["calendar.example:443"]}"#,
    );
    let token = dir.path().join("admin.token");
    std::fs::write(&token, "original-secret\n").unwrap();
    let env_b = env_for(
        &profiles,
        &url,
        &dir.path().join("b").join("g.sock"),
        &[
            ("BOT_HOST_CONFIG", wider.to_str().unwrap()),
            ("BOT_ADMIN_TOKEN_FILE", token.to_str().unwrap()),
        ],
    );
    let mut blocked = Server(
        Command::new(BOT)
            .args(["serve", "--role", "admin", "--activate"])
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
            .envs(refs(&env_b))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn admin"),
    );
    wait_for("new process did not publish token file", || {
        std::fs::read_to_string(&token).is_ok_and(|text| text != "original-secret\n")
    });
    let dead_token = std::fs::read_to_string(&token).unwrap();
    assert_eq!(
        schema.active(),
        operating,
        "active revision must not change before commit"
    );

    // SIGKILL before commit leaves no chance to run Drop.
    blocked.0.kill().unwrap();
    blocked.0.wait().unwrap();
    drop(holder);
    assert_eq!(
        std::fs::read_to_string(&token).unwrap(),
        dead_token,
        "dead process token remains until next claim"
    );
    let leftovers = || -> Vec<String> {
        std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| {
                name.contains(".bak-") || name.contains(".tmp-") || name.ends_with(".journal")
            })
            .collect()
    };
    assert_eq!(
        leftovers().len(),
        2,
        "backup and recovery journal must remain: {:?}",
        leftovers()
    );
    assert_eq!(schema.active(), operating);

    // Next start claims the lock, replays the journal to restore the original file, then fails on a busy port.
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = dir.path().join("busy.json");
    std::fs::write(
        &busy,
        host_json(
            my_uid(),
            &pkg,
            "",
            &format!(
                r#", "admin": {{"bind": "{}"}}, "egress": {{"personal": ["calendar.example:443"]}}"#,
                squatter.local_addr().unwrap()
            ),
        ),
    )
    .unwrap();
    let env_c = env_for(
        &profiles,
        &url,
        &dir.path().join("c").join("g.sock"),
        &[
            ("BOT_HOST_CONFIG", busy.to_str().unwrap()),
            ("BOT_ADMIN_TOKEN_FILE", token.to_str().unwrap()),
        ],
    );
    let failed = bot(&refs(&env_c), &["serve", "--role", "admin", "--activate"]);
    assert!(!failed.status.success(), "{}", text(&failed));
    assert_eq!(
        std::fs::read_to_string(&token).unwrap(),
        "original-secret\n",
        "original file must be restored from backup"
    );
    assert_eq!(
        leftovers(),
        Vec::<String>::new(),
        "backup and journal must be cleaned up"
    );
    assert_eq!(
        schema.active(),
        operating,
        "failed start must not change active revision"
    );
    drop((squatter, server));
}
