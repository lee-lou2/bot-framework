//! Process-level tests for `bot doctor`: offline checks, no secret/URL leaks, exit code 1 on failure, and parity with runtime rules.
use crate::process_support::*;
use std::{fs, net::TcpListener, path::PathBuf};

const PASSWORD: &str = "pw-SENTINEL-4f1c9";
const SECRET: &str = "value-SENTINEL-7a2e0";

struct Work {
    dir: tempfile::TempDir,
}

impl Work {
    fn new(template: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let made = bot(
            &[],
            &[
                "init",
                "demo",
                "--template",
                template,
                "--dir",
                dir.path().to_str().unwrap(),
            ],
        );
        assert!(made.status.success(), "{}", text(&made));
        Self { dir }
    }

    fn profiles(&self) -> PathBuf {
        self.dir.path().join("demo/profiles")
    }

    fn host(&self) -> PathBuf {
        self.dir.path().join("demo/host.json")
    }

    fn profile_yaml(&self) -> PathBuf {
        self.profiles().join("demo/profile.yaml")
    }

    fn doctor(&self, extra_env: &[(&str, &str)], args: &[&str]) -> std::process::Output {
        let (profiles, host) = (self.profiles(), self.host());
        let mut env = vec![
            ("BOT_PROFILES", profiles.to_str().unwrap()),
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
        ];
        env.extend_from_slice(extra_env);
        let mut full = vec!["doctor"];
        full.extend_from_slice(args);
        bot(&env, &full)
    }
}

#[test]
fn a_healthy_setup_passes_without_touching_the_network_and_marks_the_connection_unchecked() {
    let work = Work::new("team");
    // Open a real listener so any unexpected network attempt shows up as an accepted connection.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "postgres://user:{PASSWORD}@127.0.0.1:{}/db",
        listener.local_addr().unwrap().port()
    );
    let out = work.doctor(&[("BOT_DATABASE_URL", url.as_str())], &[]);
    let shown = text(&out);
    assert!(out.status.success(), "{shown}");
    for line in [
        "[PASS] Host config",
        "[PASS] Model providers",
        "[PASS] Tool servers",
        "[PASS] Profiles — 1 profile(s) passed validation",
        "[PASS] Entrances",
        "[PASS] Secrets — no secrets required",
        "[PASS] Database — URL configured",
        "[SKIP] Database connection",
        "Summary: 7 pass · 0 warn · 0 fail · 1 skipped",
    ] {
        assert!(shown.contains(line), "{line}\n{shown}");
    }
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "default doctor check must not open network connections"
    );
    for hidden in [PASSWORD, "127.0.0.1"] {
        assert!(!shown.contains(hidden), "must not print address: {shown}");
    }
    assert_eq!(
        text(&work.doctor(&[("BOT_DATABASE_URL", url.as_str())], &[])),
        shown
    );
}

#[test]
fn a_missing_database_is_a_failure_with_guidance_and_the_rest_is_still_reported() {
    let work = Work::new("personal");
    let out = work.doctor(&[], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("[FAIL] Database — BOT_DATABASE_URL is not set"),
        "{stdout}"
    );
    assert!(stdout.contains("[PASS] Profiles"), "{stdout}");
    assert!(
        stdout.contains("[SKIP] Database connection — skipped because URL is missing"),
        "{stdout}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("bot: 1 check(s) failed"));
    let bad = format!("mysql://u:{PASSWORD}@h/db");
    let out = work.doctor(&[("BOT_DATABASE_URL", bad.as_str())], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out).contains("BOT_DATABASE_URL has invalid format")
            && !text(&out).contains(PASSWORD)
    );
}

#[test]
fn check_db_reports_a_failed_connection_without_printing_the_address() {
    let work = Work::new("personal");
    // Accept-then-drop fails immediately instead of retrying ECONNREFUSED.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || listener.incoming().for_each(drop));
    let url = format!("postgres://user:{PASSWORD}@127.0.0.1:{port}/db");
    let out = work.doctor(&[("BOT_DATABASE_URL", url.as_str())], &["--check-db"]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(
        shown.contains("[FAIL] Database connection — failed to connect"),
        "{shown}"
    );
    for hidden in [PASSWORD, "127.0.0.1"] {
        assert!(!shown.contains(hidden), "{shown}");
    }
}

#[test]
fn a_broken_host_file_fails_with_guidance_and_skips_what_depends_on_it() {
    let work = Work::new("personal");
    fs::write(work.host(), "{ not json").unwrap();
    let out = work.doctor(&[("BOT_DATABASE_URL", "postgres://u@localhost/db")], &[]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(shown.contains("[FAIL] Host config"), "{shown}");
    assert!(
        shown.to_ascii_lowercase().contains("json object"),
        "{shown}"
    );
    assert!(
        shown.contains("[SKIP] Profiles — skipped because host config is invalid"),
        "{shown}"
    );
    assert!(shown.contains("[SKIP] Secrets"), "{shown}");
    // `--host` takes precedence over `BOT_HOST_CONFIG`.
    let gone = work.dir.path().join("gone.json");
    let out = work.doctor(&[], &["--host", gone.to_str().unwrap()]);
    assert!(text(&out).contains("[FAIL] Host config"), "{}", text(&out));
    fs::write(work.host(), r#"{"version": 1, "surprise": true}"#).unwrap();
    assert!(
        text(&work.doctor(&[], &[]))
            .to_ascii_lowercase()
            .contains("unknown")
    );
    let good = Work::new("personal");
    let out = work.doctor(
        &[("BOT_DATABASE_URL", "postgres://u@localhost/db")],
        &[
            "--host",
            good.host().to_str().unwrap(),
            "--profiles",
            good.profiles().to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn missing_credentials_are_listed_by_name_and_values_are_never_printed() {
    let work = Work::new("personal");
    let yaml = fs::read_to_string(work.profile_yaml()).unwrap();
    fs::write(
        work.profile_yaml(),
        yaml.replace("secrets: []", "secrets: [alpha-key, beta-key]"),
    )
    .unwrap();
    let out = work.doctor(
        &[
            ("BOT_DATABASE_URL", "postgres://u@localhost/db"),
            ("BOT_SECRET__demo__alpha_key", SECRET),
        ],
        &[],
    );
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(shown.contains("[FAIL] Secrets"), "{shown}");
    assert!(
        shown.contains("beta-key"),
        "missing secret must be listed by name: {shown}"
    );
    assert!(shown.contains("BOT_SECRET__demo__beta_key"), "{shown}");
    assert!(
        !shown.contains("alpha-key"),
        "present secret must not be listed as missing: {shown}"
    );
    assert!(!shown.contains(SECRET), "secret value leaked: {shown}");
    let out = work.doctor(
        &[
            ("BOT_DATABASE_URL", "postgres://u@localhost/db"),
            ("BOT_SECRET__demo__alpha_key", SECRET),
            ("BOT_SECRET__demo__beta_key", SECRET),
        ],
        &[],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("[PASS] Secrets — all 2 required secret(s) present"),
        "{}",
        text(&out)
    );
    assert!(!text(&out).contains(SECRET));
}

#[test]
fn providers_without_a_runner_or_a_model_vm_fail_and_only_names_are_shown() {
    let work = Work::new("personal");
    let yaml = fs::read_to_string(work.profile_yaml()).unwrap();
    fs::write(
        work.profile_yaml(),
        yaml.replace("secrets: []", "secrets: [model-key]"),
    )
    .unwrap();
    let host = format!(
        r#"{{"version": 1,
  "gateway": {{"cli": {{"accounts": {{"owner": {{"uid": {}}}}}}}}},
  "models": {{"demo": ["assistant", "real", "ghost"]}},
  "providers": {{"assistant": {{"kind": "demo"}},
    "real": {{"kind": "claude-cli", "model": "synthetic-model", "endpoint": "https://api.example.test/v1?k=x", "secret": "model-key"}}}}}}"#,
        my_uid()
    );
    fs::write(work.host(), &host).unwrap();
    let out = work.doctor(&[("BOT_DATABASE_URL", "postgres://u@localhost/db")], &[]);
    assert!(text(&out).contains("[FAIL] Host config"), "{}", text(&out));
    fs::write(work.host(), host.replace("/v1?k=x", "/v1")).unwrap();
    let out = work.doctor(&[("BOT_DATABASE_URL", "postgres://u@localhost/db")], &[]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    for expected in [
        "[FAIL] Model providers",
        "ghost",
        "sandbox.apple",
        "real: claude-cli · model synthetic-model · upstream api.example.test · secret model-key",
        "[FAIL] Secrets",
        "model-key",
    ] {
        assert!(shown.contains(expected), "{expected}\n{shown}");
    }
    assert!(
        !shown.contains("/v1"),
        "upstream path must not be printed: {shown}"
    );
}

#[test]
fn without_a_host_file_it_says_it_uses_the_demo_host_without_failing() {
    let profiles = examples();
    let out = bot(
        &[
            ("BOT_PROFILES", profiles.to_str().unwrap()),
            ("BOT_DATABASE_URL", "postgres://u@localhost/db"),
        ],
        &["doctor"],
    );
    let shown = text(&out);
    assert!(out.status.success(), "{shown}");
    assert!(
        shown.contains(
            "[WARN] Host config — neither BOT_HOST_CONFIG nor --host is set; using the built-in demo host"
        ),
        "{shown}"
    );
    assert!(
        shown.contains("Summary: 6 pass · 1 warn · 0 fail · 1 skipped"),
        "{shown}"
    );
}

#[test]
fn driver_warnings_never_carry_address_values() {
    let work = Work::new("personal");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || listener.incoming().for_each(drop));
    let url = format!("postgres://user@127.0.0.1:{port}/db?sslpassword={SECRET}");
    for args in [&[][..], &["--check-db"][..]] {
        let out = work.doctor(
            &[("BOT_DATABASE_URL", url.as_str()), ("BOT_LOG", "trace")],
            args,
        );
        let shown = text(&out);
        assert!(!shown.contains(SECRET), "{args:?}: {shown}");
        assert!(!shown.contains("sslpassword"), "{args:?}: {shown}");
    }
}

#[test]
fn a_malformed_address_fails_without_connecting_or_echoing_it() {
    let work = Work::new("personal");
    for bad in [
        format!("postgres://user:{PASSWORD}@localhost:not-a-port/db"),
        format!("postgres://user:{PASSWORD}@localhost/db?sslmode=bogus"),
    ] {
        let out = work.doctor(&[("BOT_DATABASE_URL", bad.as_str())], &[]);
        let shown = text(&out);
        assert_eq!(out.status.code(), Some(1), "{shown}");
        assert!(
            shown.contains("[FAIL] Database — BOT_DATABASE_URL has invalid format"),
            "{shown}"
        );
        for hidden in [PASSWORD, "not-a-port", "bogus"] {
            assert!(!shown.contains(hidden), "{hidden}: {shown}");
        }
    }
}

fn with_host_keys(work: &Work, edit: impl FnOnce(&mut serde_json::Value)) {
    let mut host: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(work.host()).unwrap()).unwrap();
    edit(&mut host);
    fs::write(work.host(), host.to_string()).unwrap();
}

#[test]
fn credentials_referenced_by_collector_headers_are_checked_by_name() {
    let work = Work::new("personal");
    with_host_keys(&work, |host| {
        host["admin"] = serde_json::json!({"trace": {"demo": {
            "endpoint": "http://127.0.0.1:4318/v1/traces",
            "headers": {"Authorization": {"secret": "collector-token"}}
        }}});
    });
    let env = [("BOT_DATABASE_URL", "postgres://u@localhost/db")];
    let out = work.doctor(&env, &[]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(shown.contains("[FAIL] Secrets"), "{shown}");
    assert!(shown.contains("collector-token"), "{shown}");
    let (profiles, host) = (work.profiles(), work.host());
    let checked = bot(
        &[
            ("BOT_PROFILES", profiles.to_str().unwrap()),
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
        ],
        &["validate"],
    );
    assert!(
        !checked.status.success(),
        "validate must share the secret list"
    );
    assert!(
        text(&checked).contains("collector-token"),
        "{}",
        text(&checked)
    );
    let out = work.doctor(
        &[
            ("BOT_DATABASE_URL", "postgres://u@localhost/db"),
            ("BOT_SECRET__demo__collector_token", SECRET),
        ],
        &[],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains(SECRET));
}

#[test]
fn a_required_state_server_fails_the_way_the_worker_refuses_it() {
    use std::os::unix::fs::PermissionsExt;
    let work = Work::new("personal");
    let server = tempfile::tempdir().unwrap();
    fs::create_dir(server.path().join("bin")).unwrap();
    let program = server.path().join("bin/server");
    fs::write(&program, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let root = fs::canonicalize(server.path()).unwrap();
    let env = [("BOT_DATABASE_URL", "postgres://u@localhost/db")];
    for (state, ok) in [("required", false), ("ephemeral", true)] {
        with_host_keys(&work, |host| {
            host["servers"] = serde_json::json!({"stateful": {
                "root": root.to_str().unwrap(), "command": ["bin/server"], "state": state
            }});
        });
        let out = work.doctor(&env, &[]);
        let shown = text(&out);
        assert_eq!(out.status.success(), ok, "{state}: {shown}");
        if ok {
            assert!(shown.contains("[PASS] Tool servers"), "{shown}");
        } else {
            assert!(shown.contains("[FAIL] Tool servers"), "{shown}");
            assert!(shown.contains("servers.stateful.state=required"), "{shown}");
        }
    }
}

#[test]
fn api_model_providers_show_credential_names_and_use_the_workers_rules() {
    let work = Work::new("personal");
    with_host_keys(&work, |host| {
        host["models"] =
            serde_json::json!({"demo": ["assistant", "claude", "gpt", "local", "gpt-new"]});
        host["providers"] = serde_json::json!({
            "assistant": {"kind": "demo"},
            "claude": {"kind": "anthropic-api", "model": "synthetic-model",
                "endpoint": "https://api.example.test/private/path", "secret": "anthropic-key"},
            "gpt": {"kind": "openai-api", "model": "synthetic-model",
                "endpoint": "https://api.example.test/v1", "secret": "openai-key", "max_tokens": 1024},
            "local": {"kind": "openai-api", "model": "synthetic-model",
                "endpoint": "http://127.0.0.1:11434/v1"},
            "gpt-new": {"kind": "openai-api", "model": "synthetic-model",
                "endpoint": "https://api.example.test/v1", "secret": "openai-key", "max_completion_tokens": 2048}
        });
    });
    let db = ("BOT_DATABASE_URL", "postgres://u@localhost/db");
    let out = work.doctor(&[db], &[]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "missing keys: {shown}");
    for expected in [
        "[PASS] Model providers — 5",
        "claude: anthropic-api · model synthetic-model · upstream api.example.test · secret anthropic-key",
        "gpt: openai-api · model synthetic-model · upstream api.example.test · secret openai-key · max tokens 1024",
        "local: openai-api · model synthetic-model · upstream 127.0.0.1:11434 · no service key (loopback only)",
        "gpt-new: openai-api · model synthetic-model · upstream api.example.test · secret openai-key · max tokens 2048 (max_completion_tokens)",
        "[FAIL] Secrets",
        "anthropic-key",
        "openai-key",
    ] {
        assert!(shown.contains(expected), "{expected}\n{shown}");
    }
    assert!(
        !shown.contains("/private/path"),
        "upstream path must not be printed: {shown}"
    );
    let out = work.doctor(
        &[
            db,
            ("BOT_SECRET__demo__anthropic_key", SECRET),
            ("BOT_SECRET__demo__openai_key", SECRET),
        ],
        &[],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(!text(&out).contains(SECRET));
    with_host_keys(&work, |host| {
        host["providers"]["local"]["endpoint"] = "https://api.example.test/v1".into();
    });
    let out = work.doctor(&[db], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("loopback"), "{}", text(&out));
}

#[test]
fn remote_mcp_servers_follow_the_connection_policy_without_connecting() {
    let work = Work::new("personal");
    let env = [("BOT_DATABASE_URL", "postgres://u@localhost/db")];
    with_host_keys(&work, |host| {
        host["servers"] = serde_json::json!({
            "notes": {"transport": "http", "url": "https://tools.example.test/mcp?secret-in-path=1",
                "auth": {"kind": "bearer", "secret": "notes-token"}},
            "local-mcp": {"transport": "http", "url": "http://127.0.0.1:8931/mcp"}
        });
    });
    let out = work.doctor(&env, &[]);
    let shown = text(&out);
    assert!(out.status.success(), "{shown}");
    for expected in [
        "[PASS] Tool servers — 2",
        "notes: remote https · tools.example.test:443 · bearer secret notes-token · DNS resolution checked at connect time",
        "local-mcp: remote http · 127.0.0.1:8931 · no auth",
    ] {
        assert!(shown.contains(expected), "{expected}\n{shown}");
    }
    assert!(
        !shown.contains("secret-in-path"),
        "path/query must not be printed: {shown}"
    );
    with_host_keys(&work, |host| {
        host["servers"]["intranet"] =
            serde_json::json!({"transport": "http", "url": "https://10.1.2.3/mcp"});
    });
    let out = work.doctor(&env, &[]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(shown.contains("[FAIL] Tool servers"), "{shown}");
    assert!(shown.contains("servers.intranet"), "{shown}");
}
