//! End-to-end tests for `bot add tool <skill>.<tool>`: creating/appending `tools.yaml`, leaving
//! `profile.yaml` untouched, schema/lock compliance, and invalid/ambiguous/symlink rejections.
use crate::process_support::*;
use bot_profile::{Limits, Schema, locate::parse_tools_yaml};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Output};

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn add(profile: &Path, args: &[&str]) -> Output {
    let mut full = vec!["add"];
    full.extend_from_slice(args);
    full.extend(["--dir", s(profile)]);
    bot(&[("BOT_DATABASE_URL", "not-a-valid-url")], &full)
}

fn init(dir: &Path, name: &str, template: &str) -> Output {
    bot(
        &[],
        &["init", name, "--template", template, "--dir", s(dir)],
    )
}

fn validate(profiles: &Path, host: &Path) -> Output {
    bot(
        &[("BOT_PROFILES", s(profiles)), ("BOT_HOST_CONFIG", s(host))],
        &["validate"],
    )
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn tools_schema() -> Schema {
    let doc: Value = serde_json::from_str(include_str!("../../../../schemas/v1/tools.schema.json"))
        .expect("tools.schema.json");
    Schema::compile_bundled(doc).expect("compile tools schema")
}

#[test]
fn adds_tools_to_new_and_existing_tools_yaml_without_touching_profile_permissions() {
    let dir = tempfile::tempdir().unwrap();
    assert!(init(dir.path(), "mybot", "personal").status.success());
    let profile = dir.path().join("mybot/profiles/mybot");
    let profile_before = fs::read(profile.join("profile.yaml")).unwrap();

    let made_skill = add(&profile, &["skill", "weather"]);
    assert!(made_skill.status.success(), "{}", text(&made_skill));
    assert!(
        text(&made_skill).contains("bot add tool weather."),
        "{}",
        text(&made_skill)
    );

    let first = add(&profile, &["tool", "weather.forecast"]);
    assert!(first.status.success(), "{}", text(&first));
    let out1 = text(&first);
    let low1 = out1.to_ascii_lowercase();
    assert!(
        low1.contains("permission") && low1.contains("host"),
        "{out1}"
    );
    assert!(out1.contains("\"weather\""), "{out1}");
    assert!(out1.contains("weather.forecast"), "{out1}");
    assert!(out1.contains("docs/first-tool.md"), "{out1}");
    assert!(
        !out1.contains("approve:"),
        "read tools should not suggest approve: {out1}"
    );

    let tools_path = profile.join("skills/weather/tools.yaml");
    assert!(tools_path.is_file());
    let commented = format!(
        "# Weather tool declarations\n{}",
        fs::read_to_string(&tools_path).unwrap()
    );
    fs::write(&tools_path, &commented).unwrap();

    let second = add(&profile, &["tool", "weather.update", "--class", "write"]);
    assert!(second.status.success(), "{}", text(&second));
    let out2 = text(&second);
    assert!(out2.contains("approve: [..., weather.update]"), "{out2}");

    let third = add(
        &profile,
        &["tool", "weather.purge", "--class", "irreversible"],
    );
    assert!(third.status.success(), "{}", text(&third));
    let out3 = text(&third);
    let low3 = out3.to_ascii_lowercase();
    assert!(
        low3.contains("always") && low3.contains("approval"),
        "{out3}"
    );

    assert_eq!(
        fs::read(profile.join("profile.yaml")).unwrap(),
        profile_before
    );

    let raw = fs::read_to_string(&tools_path).unwrap();
    assert!(raw.starts_with("# Weather tool declarations\n"), "{raw}");
    let doc = parse_tools_yaml(
        "weather",
        "skills/weather/tools.yaml",
        raw.as_bytes(),
        None,
        true,
        &Limits::default(),
    )
    .expect("tools.yaml rules pass");
    assert_eq!(doc.server, "weather");
    assert_eq!(doc.tool_ids, ["forecast", "update", "purge"]);
    assert!(tools_schema().validate(&doc.value).is_empty());
}

#[test]
fn following_printed_guidance_passes_bot_validate() {
    let dir = tempfile::tempdir().unwrap();
    assert!(init(dir.path(), "team-bot", "team").status.success());
    let base = dir.path().join("team-bot");
    let profile = base.join("profiles/team-bot");

    assert!(
        add(&profile, &["skill", "notes", "--agent", "main/developer"])
            .status
            .success()
    );
    let added = add(
        &profile,
        &[
            "tool",
            "notes.read",
            "--agent",
            "main/developer",
            "--server",
            "notes-srv",
        ],
    );
    assert!(added.status.success(), "{}", text(&added));
    let out = text(&added);
    assert!(out.contains("main/developer: [..., notes.read]"), "{out}");
    assert!(out.contains("\"notes-srv\""), "{out}");

    let added2 = add(
        &profile,
        &[
            "tool",
            "notes.save",
            "--class",
            "write",
            "--agent",
            "main/developer",
        ],
    );
    assert!(added2.status.success(), "{}", text(&added2));
    assert!(text(&added2).contains("approve: [..., notes.save]"));

    let srv_root = dir.path().join("servers/notes-srv");
    fs::create_dir_all(srv_root.join("bin")).unwrap();
    let srv_bin = srv_root.join("bin/server");
    fs::write(&srv_bin, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&srv_bin, fs::Permissions::from_mode(0o755)).unwrap();

    let host_path = base.join("host.json");
    let mut host: Value = serde_json::from_str(&fs::read_to_string(&host_path).unwrap()).unwrap();
    host["servers"] = json!({
        "notes-srv": {
            "root": srv_root.to_str().unwrap(),
            "command": ["bin/server", "--stdio"]
        }
    });
    fs::write(&host_path, serde_json::to_string_pretty(&host).unwrap()).unwrap();

    let prof_path = profile.join("profile.yaml");
    let prof = fs::read_to_string(&prof_path)
        .unwrap()
        .replace(
            "lead: [run, memory.search, memory.save, memory.forget]",
            "lead: [run, memory.search, memory.save, memory.forget, notes.read, notes.save]",
        )
        .replace(
            "scopes: [run, memory.search, memory.save, memory.forget]",
            "scopes: [run, memory.search, memory.save, memory.forget, notes.read, notes.save]",
        )
        .replace(
            "main/developer: [run]",
            "main/developer: [run, notes.read, notes.save]",
        )
        .replace("approve: []", "approve: [notes.save]");
    fs::write(&prof_path, prof).unwrap();

    let checked = validate(&base.join("profiles"), &host_path);
    let report = text(&checked);
    assert!(checked.status.success(), "{report}");
    assert!(report.contains("Validation passed"), "{report}");
    assert!(!report.to_ascii_lowercase().contains("warning"), "{report}");
}

#[test]
fn handles_yaml_keywords_empty_and_flow_tools_and_lock_backed_skills() {
    let dir = tempfile::tempdir().unwrap();
    assert!(init(dir.path(), "kw", "personal").status.success());
    let profile = dir.path().join("kw/profiles/kw");

    // YAML keywords (`null`, `true`, `false`) must be quoted so parsers read them as strings.
    assert!(add(&profile, &["skill", "null"]).status.success());
    let out = add(&profile, &["tool", "null.true", "--server", "false"]);
    assert!(out.status.success(), "{}", text(&out));
    let raw = fs::read(profile.join("skills/null/tools.yaml")).unwrap();
    let doc = parse_tools_yaml(
        "null",
        "skills/null/tools.yaml",
        &raw,
        None,
        true,
        &Limits::default(),
    )
    .expect("YAML keyword names pass");
    assert_eq!(doc.server, "false");
    assert_eq!(doc.tool_ids, ["true"]);

    assert!(add(&profile, &["skill", "flow"]).status.success());
    let flow_tools = profile.join("skills/flow/tools.yaml");
    fs::write(&flow_tools, "version: 1\nserver: flow\ntools: []\n").unwrap();
    let added = add(&profile, &["tool", "flow.first"]);
    assert!(added.status.success(), "{}", text(&added));
    let doc = parse_tools_yaml(
        "flow",
        "skills/flow/tools.yaml",
        &fs::read(&flow_tools).unwrap(),
        None,
        true,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(doc.tool_ids, ["first"]);

    // Preserves comments when `tools:` precedes `server:`.
    fs::write(
        &flow_tools,
        "# Header comment\nversion: 1\ntools:\n  # First tool\n  - id: first\n    class: read\n    scopes: [flow.first]\n    input: {type: object, properties: {}, additionalProperties: false}\n# Server comment\nserver: flow\n",
    )
    .unwrap();
    let added2 = add(&profile, &["tool", "flow.second"]);
    assert!(added2.status.success(), "{}", text(&added2));
    let merged_raw = fs::read_to_string(&flow_tools).unwrap();
    assert!(
        merged_raw.contains("# Header comment")
            && merged_raw.contains("# First tool")
            && merged_raw.contains("# Server comment"),
        "all comments must be preserved:\n{merged_raw}"
    );
    let doc2 = parse_tools_yaml(
        "flow",
        "skills/flow/tools.yaml",
        merged_raw.as_bytes(),
        None,
        true,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(doc2.tool_ids, ["first", "second"]);

    // Non-empty flow arrays are left untouched with a manual snippet so comments are never dropped.
    let flow_array = "# Flow comment\nversion: 1\ntools: [{id: first, class: read, scopes: [flow.first], input: {type: object, properties: {}, additionalProperties: false}}]\nserver: flow\n";
    fs::write(&flow_tools, flow_array).unwrap();
    let refused_flow = add(&profile, &["tool", "flow.second"]);
    assert!(!refused_flow.status.success(), "{}", text(&refused_flow));
    let flow_msg = text(&refused_flow);
    assert!(
        flow_msg.contains("tools: [...]") && flow_msg.contains("- id: second"),
        "{flow_msg}"
    );
    assert_eq!(
        fs::read_to_string(&flow_tools).unwrap(),
        flow_array,
        "flow-array file must remain unmodified"
    );

    let lock_input = json!({"type": "object", "properties": {}, "additionalProperties": false});
    let sha =
        bot_core::mcp::definition_sha256("remote_only", "remote lookup", &lock_input).unwrap();
    let lock_yaml = format!(
        "version: 1\nserver: flow\ntools:\n  - id: remote-only\n    name: remote_only\n    description: \"remote lookup\"\n    input_schema: {}\n    sha256: \"{sha}\"\n",
        serde_json::to_string(&lock_input).unwrap()
    );
    fs::write(profile.join("skills/flow/tools.lock.yaml"), &lock_yaml).unwrap();
    fs::write(
        &flow_tools,
        "version: 1\nserver: flow\ntools:\n  - id: remote-only\n    name: remote_only\n    class: read\n    scopes: [flow.remote-only]\n",
    )
    .unwrap();
    let added3 = add(&profile, &["tool", "flow.local"]);
    assert!(added3.status.success(), "{}", text(&added3));
    let updated_tools = fs::read(&flow_tools).unwrap();
    let skill_md = fs::read(profile.join("skills/flow/SKILL.md")).unwrap();
    let doc3 = parse_tools_yaml(
        "flow",
        "skills/flow/tools.yaml",
        &updated_tools,
        Some(lock_yaml.as_bytes()),
        true,
        &Limits::default(),
    )
    .expect("tools::parse passes with lock file");
    assert_eq!(doc3.tool_ids, ["remote-only", "local"]);
    assert!(tools_schema().validate(&doc3.value).is_empty());
    bot_profile::locate::check_skill(
        "flow",
        &skill_md,
        &updated_tools,
        lock_yaml.as_bytes(),
        &Limits::default(),
    )
    .expect("full skill validation passes");

    assert!(add(&profile, &["skill", "locked"]).status.success());
    fs::write(
        profile.join("skills/locked/tools.lock.yaml"),
        lock_yaml.replace("server: flow", "server: other"),
    )
    .unwrap();
    let bad_lock = add(&profile, &["tool", "locked.get", "--server", "locked"]);
    assert!(!bad_lock.status.success());
    assert!(
        text(&bad_lock).contains("lock-server-mismatch"),
        "{}",
        text(&bad_lock)
    );
    assert!(!profile.join("skills/locked/tools.yaml").exists());
}

#[test]
fn rejects_bad_names_reserved_ids_duplicates_missing_or_ambiguous_skills_and_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    assert!(init(dir.path(), "team-bot", "team").status.success());
    let profile = dir.path().join("team-bot/profiles/team-bot");

    for bad in [
        "noperiod",
        "Bad.tool",
        "skill.Bad",
        "a.b.c",
        ".tool",
        "skill.",
    ] {
        let out = add(&profile, &["tool", bad]);
        assert!(!out.status.success(), "{bad} must be rejected");
        assert!(
            text(&out).contains("agenda.search"),
            "{bad}: {}",
            text(&out)
        );
    }
    let bad_server = add(&profile, &["tool", "weather.get", "--server", "BadServer"]);
    assert!(!bad_server.status.success());
    assert!(text(&bad_server).contains("--server my-server"));

    for reserved in [
        "memory.share",
        "schedule.create",
        "schedule.list",
        "schedule.cancel",
        "schedule.deliver",
    ] {
        let out = add(&profile, &["tool", reserved]);
        assert!(!out.status.success(), "{reserved} must be rejected");
        let low = text(&out).to_ascii_lowercase();
        assert!(
            low.contains("built-in") || low.contains("builtin") || low.contains("reserved"),
            "{reserved}: {}",
            text(&out)
        );
    }
    assert!(add(&profile, &["skill", "memory"]).status.success());
    assert!(add(&profile, &["tool", "memory.note"]).status.success());

    let missing_skill = add(&profile, &["tool", "ghost.list"]);
    assert!(!missing_skill.status.success());
    assert!(text(&missing_skill).contains("bot add skill ghost"));
    let missing_agent = add(&profile, &["tool", "ghost.list", "--agent", "main/nope"]);
    assert!(!missing_agent.status.success());
    assert!(text(&missing_agent).contains("bot add agent main/nope"));

    let dup = add(&profile, &["tool", "memory.note"]);
    assert!(!dup.status.success());
    let dup_msg = text(&dup);
    let dup_low = dup_msg.to_ascii_lowercase();
    assert!(
        dup_low.contains("already") && dup_low.contains("overwrite"),
        "{dup_msg}"
    );
    let mismatch = add(&profile, &["tool", "memory.other", "--server", "other-srv"]);
    assert!(!mismatch.status.success());
    let mismatch_msg = text(&mismatch);
    assert!(
        mismatch_msg.contains("`memory`") && mismatch_msg.contains("--server other-srv"),
        "{mismatch_msg}"
    );

    assert!(
        add(&profile, &["skill", "ops", "--agent", "main/developer"])
            .status
            .success()
    );
    assert!(
        add(&profile, &["skill", "ops", "--agent", "main/pm"])
            .status
            .success()
    );
    let ambiguous = add(&profile, &["tool", "ops.check"]);
    assert!(!ambiguous.status.success());
    let msg = text(&ambiguous);
    assert!(
        msg.contains("`ops`")
            && msg.contains("--agent main/developer")
            && msg.contains("--agent main/pm"),
        "{msg}"
    );
    assert!(
        add(
            &profile,
            &["tool", "ops.check", "--agent", "main/developer"]
        )
        .status
        .success()
    );
    assert!(
        profile
            .join("agents/main/agents/developer/skills/ops/tools.yaml")
            .is_file()
    );

    let outside = dir.path().join("outside.yaml");
    fs::write(&outside, "version: 1\nserver: ops\ntools: []\n").unwrap();
    let link_target = profile.join("agents/main/agents/pm/skills/ops/tools.yaml");
    std::os::unix::fs::symlink(&outside, &link_target).unwrap();
    let symlink_out = add(&profile, &["tool", "ops.check", "--agent", "main/pm"]);
    assert!(!symlink_out.status.success());
    assert!(
        text(&symlink_out).to_ascii_lowercase().contains("link"),
        "{}",
        text(&symlink_out)
    );
    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        "version: 1\nserver: ops\ntools: []\n",
        "symlink target outside profile must remain untouched"
    );
}
