//! Process-level test for missing tool secrets: `bot validate` fails strictly, while `worker`
//! still starts with a name-only warning and refuses affected calls pre-ledger (`secret_missing`)
//! before opening any approval card. Skips when PG, `srt`, or `python3` is missing.
mod process_support;
use process_support::*;
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Copies the example profile and binds `calendar-writer` as `CALENDAR_TOKEN` on `agenda.delete`.
fn profiles_needing_a_secret(dir: &Path) -> PathBuf {
    let copy = dir.join("profiles");
    copy_dir(&examples(), &copy);
    let profile = copy.join("personal/profile.yaml");
    let text = std::fs::read_to_string(&profile).unwrap();
    assert!(text.contains("secrets: []"));
    std::fs::write(
        &profile,
        text.replacen("secrets: []", "secrets: [calendar-writer]", 1),
    )
    .unwrap();
    let tools = copy.join("personal/agents/main/skills/agenda/tools.yaml");
    let text = std::fs::read_to_string(&tools).unwrap();
    let (list, delete) = text
        .split_once("  - id: delete")
        .expect("delete tool declaration");
    assert!(delete.contains("secrets: {}"));
    std::fs::write(
        &tools,
        format!(
            "{list}  - id: delete{}",
            delete.replacen(
                "secrets: {}",
                "secrets: {CALENDAR_TOKEN: calendar-writer}",
                1
            )
        ),
    )
    .unwrap();
    copy
}

fn host(dir: &Path) -> PathBuf {
    let server = std::fs::canonicalize(repo().join("examples/servers/demo-agenda")).unwrap();
    let path = dir.join("host.json");
    std::fs::write(
        &path,
        host_json(
            my_uid(),
            &server,
            "",
            r#", "providers": {"assistant": {"kind": "demo"}}"#,
        ),
    )
    .unwrap();
    path
}

fn spawn_logging(envs: &[(&str, &str)], role: &str, log: &Path) -> Server {
    Server(
        Command::new(BOT)
            .args(["serve", "--role", role])
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
            .envs(envs.iter().copied())
            .stdout(Stdio::null())
            .stderr(File::create(log).unwrap())
            .spawn()
            .expect("spawn serve"),
    )
}

#[test]
fn a_missing_tool_secret_fails_validate_but_never_stops_the_worker_and_the_call_is_refused_before_any_card()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("skipped: BOT_TEST_DATABASE_URL not set");
        return;
    };
    if !have_srt_and_python() {
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("t")
        .tempdir_in("/tmp")
        .unwrap();
    let profiles = profiles_needing_a_secret(dir.path());
    let host = host(dir.path());
    let socket = dir.path().join("run").join("g.sock");
    let installs = dir.path().join("installs");
    let url = schema.url();
    let env = |extra: &[(&str, &str)]| {
        let mut pairs = vec![
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_INSTALLS", installs.to_str().unwrap()),
        ];
        pairs.extend_from_slice(extra);
        env_for(&profiles, &url, &socket, &pairs)
    };

    let validated = bot(&refs(&env(&[])), &["validate"]);
    assert_eq!(validated.status.code(), Some(1), "{}", text(&validated));
    let message = text(&validated);
    assert!(
        message.contains("calendar-writer")
            && message.contains("BOT_SECRET__personal__calendar_writer"),
        "{message}"
    );

    let without = env(&[]);
    let _gateway = start_gateway_roles(&refs(&without), "gateway", &socket);
    wait_for("active revision was not published", || {
        !schema.active().is_empty()
    });
    let log = dir.path().join("worker.log");
    let worker = spawn_logging(&refs(&without), "worker", &log);
    let warned = || std::fs::read_to_string(&log).unwrap_or_default();
    wait_for("worker did not emit missing tool secret warning", || {
        warned().contains("calendar-writer")
    });
    let warning = warned();
    let warning_low = warning.to_ascii_lowercase();
    assert!(
        warning.contains("agenda.delete")
            && (warning_low.contains("unusable")
                || warning_low.contains("unavailable")
                || warning_low.contains("missing")),
        "{warning}"
    );

    // Unaffected read tools keep working while the missing-secret delete tool is refused.
    let listed = bot(
        &refs(&without),
        &["chat", "--request-id", "t0", "Show today's agenda"],
    );
    assert!(listed.status.success(), "{}", text(&listed));
    assert!(text(&listed).contains("Walk"), "{}", text(&listed));

    // Must complete promptly without parking on an approval card.
    let refused = bot_within(
        &refs(&without),
        &["chat", "--request-id", "t1", "Delete the walk event"],
        std::time::Duration::from_secs(60),
    )
    .expect("delete request did not finish — refusal must happen before any approval card");
    assert!(refused.status.success(), "{}", text(&refused));
    assert!(text(&refused).contains("was kept"), "{}", text(&refused));
    assert_eq!(
        schema.query("select count(*) from {s}.bot_approvals"),
        "0",
        "must not ask a human to approve an unrunnable call"
    );
    assert_eq!(
        schema.query("select count(*) from {s}.bot_calls where tool = 'agenda.delete'"),
        "0",
        "pre-ledger refusal must not insert a call row"
    );
    assert_eq!(
        schema.query(
            "select count(*) from {s}.bot_audit where action = 'call.refuse' and target = 'agenda.delete' and verdict = 'secret_missing'"
        ),
        "1",
        "refusal must be audited with fixed code secret_missing"
    );
    assert!(
        worker_alive(&worker),
        "missing secret for one tool must not stop the worker"
    );
    drop(worker);

    // Providing the secret restores normal approval-card flow without logging the value.
    let value = "value-for-the-calendar-token";
    let filled = env(&[("BOT_SECRET__personal__calendar_writer", value)]);
    let log = dir.path().join("worker2.log");
    let _worker = spawn_logging(&refs(&filled), "worker", &log);
    let mut asking = Command::new(BOT)
        .args(["chat", "--request-id", "t2", "Delete the walk event"])
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .envs(refs(&filled))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chat");
    wait_for(
        "approval card did not open once secret was provided",
        || schema.query("select count(*) from {s}.bot_approvals where state = 'pending'") == "1",
    );
    assert_eq!(
        schema.query("select count(*) from {s}.bot_calls where tool = 'agenda.delete'"),
        "1"
    );
    let second = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(!second.contains("calendar-writer"), "{second}");
    assert!(!second.contains(value));
    let _ = asking.kill();
    let _ = asking.wait();
}

fn bot_within(
    envs: &[(&str, &str)],
    args: &[&str],
    limit: std::time::Duration,
) -> Option<std::process::Output> {
    let mut child = Command::new(BOT)
        .args(args)
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .envs(envs.iter().copied())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bot");
    let deadline = std::time::Instant::now() + limit;
    loop {
        if child.try_wait().expect("wait status").is_some() {
            return child.wait_with_output().ok();
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn worker_alive(server: &Server) -> bool {
    let pid = server.0.id().to_string();
    Command::new("/bin/kill")
        .args(["-0", &pid])
        .status()
        .is_ok_and(|status| status.success())
}
