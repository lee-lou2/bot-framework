//! `bot add tool <스킬>.<도구>` 종단 시험:
//! - 새 `tools.yaml` 생성과 기존 `tools.yaml` 덧붙이기(read·write·irreversible)
//! - `profile.yaml` 권한 상한을 건드리지 않고 안내만 출력
//! - 산출물이 `schemas/v1/tools.schema.json`과 잠금 규칙을 따르고, 안내대로 설정하면 `bot validate` 통과
//! - 중복·잘못된 이름·예약 내장 ID·없는 스킬·모호한 스킬·서버 불일치·링크 거절
mod process_support;
use bot_profile::{Limits, Schema, locate::parse_tools_yaml};
use process_support::*;
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
    let doc: Value = serde_json::from_str(include_str!("../../../schemas/v1/tools.schema.json"))
        .expect("tools.schema.json");
    Schema::compile_bundled(doc).expect("tools 스키마 컴파일")
}

#[test]
fn adds_tools_to_new_and_existing_tools_yaml_without_touching_profile_permissions() {
    let dir = tempfile::tempdir().unwrap();
    assert!(init(dir.path(), "mybot", "personal").status.success());
    let profile = dir.path().join("mybot/profiles/mybot");
    let profile_before = fs::read(profile.join("profile.yaml")).unwrap();

    // 스킬 생성 뒤 첫 도구(기본 class=read)
    let made_skill = add(&profile, &["skill", "weather"]);
    assert!(made_skill.status.success(), "{}", text(&made_skill));
    assert!(text(&made_skill).contains("bot add tool weather.<도구>"));

    let first = add(&profile, &["tool", "weather.forecast"]);
    assert!(first.status.success(), "{}", text(&first));
    let out1 = text(&first);
    assert!(
        out1.contains("권한과 호스트 설정은 자동으로 바꾸지 않았어요"),
        "{out1}"
    );
    assert!(out1.contains("\"weather\""), "{out1}");
    assert!(out1.contains("weather.forecast"), "{out1}");
    assert!(out1.contains("docs/first-tool.md"), "{out1}");
    assert!(
        !out1.contains("approve:"),
        "read 도구는 approve 안내 없음: {out1}"
    );

    let tools_path = profile.join("skills/weather/tools.yaml");
    assert!(tools_path.is_file());
    // 주석을 넣어 두고 두 번째 도구(write)를 덧붙여 주석이 보존되는지 확인
    let commented = format!(
        "# 날씨 도구 선언\n{}",
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
    assert!(out3.contains("항상 사람 승인을 거쳐요"), "{out3}");

    // profile.yaml은 한 바이트도 바뀌지 않았다
    assert_eq!(
        fs::read(profile.join("profile.yaml")).unwrap(),
        profile_before
    );

    // 산출물은 주석을 보존하고 tools.schema.json + tools::parse(잠금 규칙 포함)를 통과한다
    let raw = fs::read_to_string(&tools_path).unwrap();
    assert!(raw.starts_with("# 날씨 도구 선언\n"), "{raw}");
    let doc = parse_tools_yaml(
        "weather",
        "skills/weather/tools.yaml",
        raw.as_bytes(),
        None,
        true,
        &Limits::default(),
    )
    .expect("tools.yaml 규칙 통과");
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
    // 하위 에이전트 스킬이면 policy.agents 안내까지 포함한다
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

    // 안내 1: 최소 실행 가능 서버를 만들고 host.json의 servers에 등록
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

    // 안내 2: profile.yaml의 roles와 policy.scopes·policy.agents·policy.approve에 허용
    let prof_path = profile.join("profile.yaml");
    let prof = fs::read_to_string(&prof_path)
        .unwrap()
        .replace("lead: [run]", "lead: [run, notes.read, notes.save]")
        .replace("scopes: [run]", "scopes: [run, notes.read, notes.save]")
        .replace(
            "main/developer: [run]",
            "main/developer: [run, notes.read, notes.save]",
        )
        .replace("approve: []", "approve: [notes.save]");
    fs::write(&prof_path, prof).unwrap();

    // 안내 4: bot validate 통과(오류 0·경고 0)
    let checked = validate(&base.join("profiles"), &host_path);
    let report = text(&checked);
    assert!(checked.status.success(), "{report}");
    assert!(report.contains("검증 통과"), "{report}");
    assert!(!report.contains("경고"), "{report}");
}

#[test]
fn handles_yaml_keywords_empty_and_flow_tools_and_lock_backed_skills() {
    let dir = tempfile::tempdir().unwrap();
    assert!(init(dir.path(), "kw", "personal").status.success());
    let profile = dir.path().join("kw/profiles/kw");

    // YAML 키워드 이름(null·true·false)도 따옴표 문자열로 안전하게 기록된다
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
    .expect("YAML 키워드 이름 통과");
    assert_eq!(doc.server, "false");
    assert_eq!(doc.tool_ids, ["true"]);

    // `tools: []` 및 플로우 배열·키 순서가 다른 기존 tools.yaml에도 안전하게 덧붙인다
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

    // 플로우 배열 + server가 뒤에 있는 변칙 파일도 정규 렌더링으로 안전하게 병합된다
    fs::write(
        &flow_tools,
        "version: 1\ntools: [{id: first, class: read, scopes: [flow.first], input: {type: object, properties: {}, additionalProperties: false}}]\nserver: flow\n",
    )
    .unwrap();
    let added2 = add(&profile, &["tool", "flow.second"]);
    assert!(added2.status.success(), "{}", text(&added2));
    let doc2 = parse_tools_yaml(
        "flow",
        "skills/flow/tools.yaml",
        &fs::read(&flow_tools).unwrap(),
        None,
        true,
        &Limits::default(),
    )
    .unwrap();
    assert_eq!(doc2.tool_ids, ["first", "second"]);

    // 기존 tools.yaml에 input 없는 원격 도구 + 유효한 tools.lock.yaml이 있을 때도 잠금 규칙을 지키며 새 도구를 더한다
    let lock_input = json!({"type": "object", "properties": {}, "additionalProperties": false});
    let sha = bot_core::mcp::definition_sha256("remote_only", "원격 조회", &lock_input).unwrap();
    let lock_yaml = format!(
        "version: 1\nserver: flow\ntools:\n  - id: remote-only\n    name: remote_only\n    description: \"원격 조회\"\n    input_schema: {}\n    sha256: \"{sha}\"\n",
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
    .expect("잠금 파일과 함께 tools::parse 통과");
    assert_eq!(doc3.tool_ids, ["remote-only", "local"]);
    assert!(tools_schema().validate(&doc3.value).is_empty());
    bot_profile::locate::check_skill(
        "flow",
        &skill_md,
        &updated_tools,
        lock_yaml.as_bytes(),
        &Limits::default(),
    )
    .expect("스킬 전체(SKILL.md + tools.yaml + tools.lock.yaml) 검증 통과");

    // 잠금 파일의 server와 다른 --server로 새 tools.yaml을 만들려 하면 쓰기 전에 거절한다
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

    // 잘못된 도구 ID·서버 이름
    for bad in [
        "noperiod",
        "Bad.tool",
        "skill.Bad",
        "a.b.c",
        ".tool",
        "skill.",
    ] {
        let out = add(&profile, &["tool", bad]);
        assert!(!out.status.success(), "{bad}는 거절돼야 함");
        assert!(
            text(&out).contains("예: agenda.search"),
            "{bad}: {}",
            text(&out)
        );
    }
    let bad_server = add(&profile, &["tool", "weather.get", "--server", "BadServer"]);
    assert!(!bad_server.status.success());
    assert!(text(&bad_server).contains("--server my-server"));

    // 호스트 내장 예약 도구 ID는 거절하되 같은 스킬의 다른 도구는 허용
    for reserved in [
        "memory.share",
        "schedule.create",
        "schedule.list",
        "schedule.cancel",
        "schedule.deliver",
    ] {
        let out = add(&profile, &["tool", reserved]);
        assert!(!out.status.success(), "{reserved}는 거절돼야 함");
        assert!(
            text(&out).contains("호스트 내장 도구 이름"),
            "{reserved}: {}",
            text(&out)
        );
    }
    assert!(add(&profile, &["skill", "memory"]).status.success());
    assert!(add(&profile, &["tool", "memory.note"]).status.success());

    // 없는 스킬·없는 에이전트
    let missing_skill = add(&profile, &["tool", "ghost.list"]);
    assert!(!missing_skill.status.success());
    assert!(text(&missing_skill).contains("bot add skill ghost"));
    let missing_agent = add(&profile, &["tool", "ghost.list", "--agent", "main/nope"]);
    assert!(!missing_agent.status.success());
    assert!(text(&missing_agent).contains("bot add agent main/nope"));

    // 중복 도구 ID 및 서버 불일치 거절
    let dup = add(&profile, &["tool", "memory.note"]);
    assert!(!dup.status.success());
    assert!(text(&dup).contains("이미") && text(&dup).contains("덮어쓰지 않아요"));
    let mismatch = add(&profile, &["tool", "memory.other", "--server", "other-srv"]);
    assert!(!mismatch.status.success());
    assert!(text(&mismatch).contains("이미 서버 `memory`"));

    // 형제 에이전트 둘에 같은 이름 스킬이 있으면 --agent 생략 시 거절하고 --agent 지정 시 성공
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
        msg.contains("여러 곳에 있어요")
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

    // 링크 tools.yaml은 따라가지 않고 거절한다
    let outside = dir.path().join("outside.yaml");
    fs::write(&outside, "version: 1\nserver: ops\ntools: []\n").unwrap();
    let link_target = profile.join("agents/main/agents/pm/skills/ops/tools.yaml");
    std::os::unix::fs::symlink(&outside, &link_target).unwrap();
    let symlink_out = add(&profile, &["tool", "ops.check", "--agent", "main/pm"]);
    assert!(!symlink_out.status.success());
    assert!(
        text(&symlink_out).contains("링크"),
        "{}",
        text(&symlink_out)
    );
    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        "version: 1\nserver: ops\ntools: []\n",
        "바깥 파일은 건드리지 않는다"
    );
}
