//! Filesystem concurrency, formatting preservation, and file-safety tests for `bot add tool`.
use crate::process_support::{BOT, bot, text};
use bot_profile::{Limits, locate::parse_tools_yaml};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

fn init_skill(dir: &Path, profile_name: &str, skill: &str) -> std::path::PathBuf {
    let init = bot(
        &[],
        &[
            "init",
            profile_name,
            "--template",
            "personal",
            "--dir",
            dir.to_str().unwrap(),
        ],
    );
    assert!(init.status.success(), "{}", text(&init));
    let profile = dir.join(profile_name).join("profiles").join(profile_name);
    let made = bot(
        &[("BOT_DATABASE_URL", "not-a-valid-url")],
        &["add", "skill", skill, "--dir", profile.to_str().unwrap()],
    );
    assert!(made.status.success(), "{}", text(&made));
    profile
}

fn add_tool(profile: &Path, args: &[&str]) -> Output {
    let mut full = vec!["add", "tool"];
    full.extend_from_slice(args);
    full.extend(["--dir", profile.to_str().unwrap()]);
    bot(&[("BOT_DATABASE_URL", "not-a-valid-url")], &full)
}

fn spawn_add_tool(profile: &Path, id: &str) -> std::process::Child {
    Command::new(BOT)
        .args(["add", "tool", id, "--dir", profile.to_str().unwrap()])
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .env("BOT_DATABASE_URL", "not-a-valid-url")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bot add tool")
}

#[test]
fn concurrent_add_tool_on_large_and_new_files_loses_zero_updates_and_leaves_no_extra_files() {
    let dir = tempfile::tempdir().unwrap();
    let profile = init_skill(dir.path(), "race", "demo");
    let skill_dir = profile.join("skills/demo");
    let tools_path = skill_dir.join("tools.yaml");

    // 1) Six concurrent appends onto an existing ~170 KB tools.yaml lose zero updates.
    let pad = "a".repeat(1400);
    let mut big =
        String::from("# Concurrent add-tool test tools.yaml\nversion: 1\nserver: demo\ntools:\n");
    for i in 0..100 {
        big.push_str(&format!("  # seed-{i}: {pad}\n"));
        big.push_str(&format!(
            "  - id: seed-{i}\n    class: read\n    scopes: [demo.seed-{i}]\n    secrets: {{}}\n    egress: []\n    input: {{type: object, properties: {{}}, additionalProperties: false}}\n"
        ));
    }
    fs::write(&tools_path, &big).unwrap();

    let children: Vec<_> = (0..6)
        .map(|i| spawn_add_tool(&profile, &format!("demo.par-{i}")))
        .collect();
    for (i, child) in children.into_iter().enumerate() {
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "par-{i} failed: {}", text(&out));
    }
    let raw = fs::read(&tools_path).unwrap();
    let doc = parse_tools_yaml(
        "demo",
        "skills/demo/tools.yaml",
        &raw,
        None,
        true,
        &Limits::default(),
    )
    .expect("concurrent append output must stay valid");
    assert_eq!(doc.tool_ids.len(), 106, "100 existing + 6 concurrent tools");
    assert!(
        std::str::from_utf8(&raw)
            .unwrap()
            .starts_with("# Concurrent add-tool test tools.yaml\n"),
        "header comment must be preserved"
    );
    for i in 0..6 {
        let want = format!("par-{i}");
        assert!(doc.tool_ids.contains(&want), "missing {want}");
    }
    let mut names: Vec<String> = fs::read_dir(&skill_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["SKILL.md", "tools.yaml"]);

    // 2) Four concurrent first-time creations when tools.yaml does not exist yet.
    let made = bot(
        &[("BOT_DATABASE_URL", "not-a-valid-url")],
        &["add", "skill", "fresh", "--dir", profile.to_str().unwrap()],
    );
    assert!(made.status.success(), "{}", text(&made));
    let fresh_children: Vec<_> = (0..4)
        .map(|i| spawn_add_tool(&profile, &format!("fresh.init-{i}")))
        .collect();
    for (i, child) in fresh_children.into_iter().enumerate() {
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "fresh init-{i} failed: {}",
            text(&out)
        );
    }
    let fresh_raw = fs::read(profile.join("skills/fresh/tools.yaml")).unwrap();
    let fresh_doc = parse_tools_yaml(
        "fresh",
        "skills/fresh/tools.yaml",
        &fresh_raw,
        None,
        true,
        &Limits::default(),
    )
    .expect("concurrent creation output must stay valid");
    assert_eq!(fresh_doc.tool_ids.len(), 4);
    for i in 0..4 {
        assert!(fresh_doc.tool_ids.contains(&format!("init-{i}")));
    }
}

#[test]
fn preserves_comments_crlf_indentless_reverse_keys_and_refuses_flow_arrays_without_rewriting() {
    let dir = tempfile::tempdir().unwrap();
    let profile = init_skill(dir.path(), "fmt", "demo");
    let tools_path = profile.join("skills/demo/tools.yaml");

    let cases: &[(&str, &str, bool, &[&str])] = &[
        (
            "block-lf",
            "# header comment\nversion: 1\nserver: demo\ntools:\n  # first tool\n  - id: first\n    class: read\n    scopes:\n      - demo.first\n    input: {type: object, properties: {}, additionalProperties: false}\n# footer comment\n",
            false,
            &["# header comment", "# first tool", "# footer comment"],
        ),
        (
            "reverse-key",
            "# reverse comment\nversion: 1\ntools:\n  # item comment\n  - id: first\n    class: read\n    scopes: [demo.first]\n    input: {type: object, properties: {}, additionalProperties: false}\n# server comment\nserver: demo\n",
            false,
            &["# reverse comment", "# item comment", "# server comment"],
        ),
        (
            "indentless",
            "# zero-indent list\nversion: 1\nserver: demo\ntools:\n- id: first\n  class: read\n  scopes: [demo.first]\n  input: {type: object, properties: {}, additionalProperties: false}\n",
            false,
            &["# zero-indent list", "\n- id: added\n"],
        ),
        (
            "block-crlf",
            "# CRLF comment\r\nversion: 1\r\nserver: demo\r\ntools:\r\n  # tool comment\r\n  - id: first\r\n    class: read\r\n    scopes: [demo.first]\r\n    input: {type: object, properties: {}, additionalProperties: false}\r\n",
            true,
            &[
                "# CRLF comment\r\n",
                "# tool comment\r\n",
                "  - id: added\r\n",
            ],
        ),
        (
            "empty-crlf",
            "# empty list comment\r\nversion: 1\r\nserver: demo\r\ntools: [] # inline comment\r\n",
            true,
            &[
                "# empty list comment\r\n",
                "tools: # inline comment\r\n",
                "  - id: added\r\n",
            ],
        ),
        (
            "no-eof-newline",
            "# no eof newline\nversion: 1\nserver: demo\ntools:\n  - id: first\n    class: read\n    scopes: [demo.first]\n    input: {type: object, properties: {}, additionalProperties: false}",
            false,
            &["# no eof newline", "  - id: added\n"],
        ),
    ];

    for &(name, initial, expect_crlf, must_contain) in cases {
        fs::write(&tools_path, initial).unwrap();
        let out = add_tool(&profile, &["demo.added"]);
        assert!(out.status.success(), "{name}: {}", text(&out));
        let updated = fs::read_to_string(&tools_path).unwrap();
        for needle in must_contain {
            assert!(
                updated.contains(needle),
                "{name}: missing `{needle}`:\n{updated}"
            );
        }
        if expect_crlf {
            let bare_lf = updated
                .as_bytes()
                .windows(2)
                .any(|w| w[1] == b'\n' && w[0] != b'\r');
            assert!(
                !bare_lf,
                "{name}: bare LF mixed into CRLF file:\n{updated:?}"
            );
        } else {
            assert!(!updated.contains('\r'), "{name}: CR mixed into LF file");
        }
        let doc = parse_tools_yaml(
            "demo",
            "skills/demo/tools.yaml",
            updated.as_bytes(),
            None,
            true,
            &Limits::default(),
        )
        .unwrap_or_else(|e| panic!("{name}: validation failed: {e:?}\n{updated}"));
        assert!(doc.tool_ids.contains(&"added".to_owned()), "{name}");
    }

    // Non-empty flow array: refuse without modifying original bytes and print manual YAML snippet.
    let flow = "# important comment\nversion: 1\nserver: demo\ntools: [{id: first, class: read, scopes: [demo.first], input: {type: object, properties: {}, additionalProperties: false}}] # tail comment\n";
    fs::write(&tools_path, flow).unwrap();
    let refused = add_tool(&profile, &["demo.added"]);
    assert!(!refused.status.success(), "{}", text(&refused));
    let msg = text(&refused);
    let lower = msg.to_ascii_lowercase();
    assert!(
        msg.contains("- id: added") && (lower.contains("preserv") || lower.contains("comment")),
        "{msg}"
    );
    assert_eq!(fs::read_to_string(&tools_path).unwrap(), flow);
}

#[cfg(unix)]
#[test]
fn preserves_file_mode_rejects_fifo_and_hardlink_and_recovers_stale_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let profile = init_skill(dir.path(), "hard", "demo");
    let skill_dir = profile.join("skills/demo");
    let tools_path = skill_dir.join("tools.yaml");

    // 1) Preserve 0600 permissions.
    let base = "version: 1\nserver: demo\ntools:\n  - id: first\n    class: read\n    scopes: [demo.first]\n    input: {type: object, properties: {}, additionalProperties: false}\n";
    fs::write(&tools_path, base).unwrap();
    fs::set_permissions(&tools_path, fs::Permissions::from_mode(0o600)).unwrap();
    let ok = add_tool(&profile, &["demo.second"]);
    assert!(ok.status.success(), "{}", text(&ok));
    let mode = fs::metadata(&tools_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "must preserve 0600 mode (got {mode:o})");

    // 2) Clean up orphaned regular `.bot-add-tool.tmp` left by an earlier crash.
    let stale_tmp = skill_dir.join(".bot-add-tool.tmp");
    fs::write(&stale_tmp, "partial write").unwrap();
    let recovered = add_tool(&profile, &["demo.third"]);
    assert!(recovered.status.success(), "{}", text(&recovered));
    assert!(!stale_tmp.exists(), "stale temp file must be cleaned up");

    // 3) Refuse symlinked `.bot-add-tool.tmp` without touching target.
    let victim = dir.path().join("victim.txt");
    fs::write(&victim, "keep").unwrap();
    std::os::unix::fs::symlink(&victim, &stale_tmp).unwrap();
    let bad_tmp = add_tool(&profile, &["demo.fourth"]);
    assert!(!bad_tmp.status.success(), "{}", text(&bad_tmp));
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
    fs::remove_file(&stale_tmp).unwrap();

    // 4) Reject hardlink (`nlink > 1`) without touching the linked file.
    let linked_outside = dir.path().join("linked.yaml");
    fs::write(&linked_outside, base).unwrap();
    fs::remove_file(&tools_path).unwrap();
    fs::hard_link(&linked_outside, &tools_path).unwrap();
    let hard = add_tool(&profile, &["demo.fifth"]);
    assert!(!hard.status.success(), "{}", text(&hard));
    let hard_msg = text(&hard).to_ascii_lowercase();
    assert!(
        hard_msg.contains("hard link") || hard_msg.contains("hardlink"),
        "{}",
        text(&hard)
    );
    assert_eq!(fs::read_to_string(&linked_outside).unwrap(), base);

    // 5) Reject FIFO immediately without blocking on open.
    fs::remove_file(&tools_path).unwrap();
    let mkfifo = Command::new("mkfifo").arg(&tools_path).status().unwrap();
    assert!(mkfifo.success());
    let started = Instant::now();
    let fifo = add_tool(&profile, &["demo.sixth"]);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "must not block on FIFO: {:?}",
        started.elapsed()
    );
    assert!(!fifo.status.success(), "{}", text(&fifo));
    assert!(
        text(&fifo).to_ascii_lowercase().contains("regular file"),
        "{}",
        text(&fifo)
    );
}
