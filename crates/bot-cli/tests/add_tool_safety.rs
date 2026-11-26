//! `bot add tool` 동시성·서식 보존·파일 안전성 시험(`srt`·DB 없이 순수 파일 시스템에서 실행).
//! - 같은 스킬의 6개 동시 덧붙이기 / 4개 동시 첫 생성에서 갱신 유실 0건 및 잠금 파일 0건
//! - 주석·`\r\n` vs `\n`·역순 키·들여쓰기 없는 목록·`tools: [] # 주석`·EOF 개행 누락 보존
//! - 비어 있지 않은 플로우 배열은 주석을 파괴하지 않고 원본 유지 + 수동 붙여넣기 조각 출력
//! - `0600` 권한 보존, FIFO 무대기 즉시 거절, 하드링크 거절, 강제 종료로 남은 `.bot-add-tool.tmp` 안전 복구
mod process_support;
use bot_profile::{Limits, locate::parse_tools_yaml};
use process_support::{BOT, bot, text};
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
        .expect("bot add tool 실행")
}

#[test]
fn concurrent_add_tool_on_large_and_new_files_loses_zero_updates_and_leaves_no_extra_files() {
    let dir = tempfile::tempdir().unwrap();
    let profile = init_skill(dir.path(), "race", "demo");
    let skill_dir = profile.join("skills/demo");
    let tools_path = skill_dir.join("tools.yaml");

    // 1) 약 170 KB 기존 tools.yaml(100개 유효 도구 + 주석)에 6개 프로세스가 동시에 도구를 덧붙여도 유실 0건
    let pad = "a".repeat(1400);
    let mut big = String::from("# 동시 추가 시험용 tools.yaml\nversion: 1\nserver: demo\ntools:\n");
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
        assert!(out.status.success(), "par-{i} 실패: {}", text(&out));
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
    .expect("동시 덧붙이기 결과 유효");
    assert_eq!(doc.tool_ids.len(), 106, "기존 100개 + 동시 6개 모두 보존");
    assert!(
        std::str::from_utf8(&raw)
            .unwrap()
            .starts_with("# 동시 추가 시험용 tools.yaml\n"),
        "동시 추가 뒤에도 주석 보존"
    );
    for i in 0..6 {
        let want = format!("par-{i}");
        assert!(doc.tool_ids.contains(&want), "{want} 누락");
    }
    // 잠금 파일이나 임시 파일이 스킬 폴더에 남지 않는다
    let mut names: Vec<String> = fs::read_dir(&skill_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["SKILL.md", "tools.yaml"]);

    // 2) `tools.yaml`이 아직 없는 스킬에 4개 프로세스가 동시에 첫 도구를 추가해도 모두 성공·보존
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
        assert!(out.status.success(), "fresh init-{i} 실패: {}", text(&out));
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
    .expect("동시 첫 생성 결과 유효");
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
            "# 머리 주석\nversion: 1\nserver: demo\ntools:\n  # 첫 도구\n  - id: first\n    class: read\n    scopes:\n      - demo.first\n    input: {type: object, properties: {}, additionalProperties: false}\n# 꼬리 주석\n",
            false,
            &["# 머리 주석", "# 첫 도구", "# 꼬리 주석"],
        ),
        (
            "reverse-key",
            "# 역순 주석\nversion: 1\ntools:\n  # 항목 주석\n  - id: first\n    class: read\n    scopes: [demo.first]\n    input: {type: object, properties: {}, additionalProperties: false}\n# 서버 주석\nserver: demo\n",
            false,
            &["# 역순 주석", "# 항목 주석", "# 서버 주석"],
        ),
        (
            "indentless",
            "# 0칸 목록\nversion: 1\nserver: demo\ntools:\n- id: first\n  class: read\n  scopes: [demo.first]\n  input: {type: object, properties: {}, additionalProperties: false}\n",
            false,
            &["# 0칸 목록", "\n- id: added\n"],
        ),
        (
            "block-crlf",
            "# CRLF 주석\r\nversion: 1\r\nserver: demo\r\ntools:\r\n  # 도구 주석\r\n  - id: first\r\n    class: read\r\n    scopes: [demo.first]\r\n    input: {type: object, properties: {}, additionalProperties: false}\r\n",
            true,
            &["# CRLF 주석\r\n", "# 도구 주석\r\n", "  - id: added\r\n"],
        ),
        (
            "empty-crlf",
            "# 빈 목록 주석\r\nversion: 1\r\nserver: demo\r\ntools: [] # 인라인 주석\r\n",
            true,
            &[
                "# 빈 목록 주석\r\n",
                "tools: # 인라인 주석\r\n",
                "  - id: added\r\n",
            ],
        ),
        (
            "no-eof-newline",
            "# 개행 없음\nversion: 1\nserver: demo\ntools:\n  - id: first\n    class: read\n    scopes: [demo.first]\n    input: {type: object, properties: {}, additionalProperties: false}",
            false,
            &["# 개행 없음", "  - id: added\n"],
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
                "{name}: `{needle}` 누락:\n{updated}"
            );
        }
        if expect_crlf {
            let bare_lf = updated
                .as_bytes()
                .windows(2)
                .any(|w| w[1] == b'\n' && w[0] != b'\r');
            assert!(!bare_lf, "{name}: CRLF 파일에 단독 LF가 섞임:\n{updated:?}");
        } else {
            assert!(!updated.contains('\r'), "{name}: LF 파일에 CR이 섞임");
        }
        let doc = parse_tools_yaml(
            "demo",
            "skills/demo/tools.yaml",
            updated.as_bytes(),
            None,
            true,
            &Limits::default(),
        )
        .unwrap_or_else(|e| panic!("{name}: 검증 실패: {e:?}\n{updated}"));
        assert!(doc.tool_ids.contains(&"added".to_owned()), "{name}");
    }

    // 비어 있지 않은 플로우 배열 + 주석: 원본 바이트 유지 + 수동 붙여넣기 YAML 출력
    let flow = "# 소중한 주석\nversion: 1\nserver: demo\ntools: [{id: first, class: read, scopes: [demo.first], input: {type: object, properties: {}, additionalProperties: false}}] # 끝 주석\n";
    fs::write(&tools_path, flow).unwrap();
    let refused = add_tool(&profile, &["demo.added"]);
    assert!(!refused.status.success(), "{}", text(&refused));
    let msg = text(&refused);
    assert!(msg.contains("- id: added") && msg.contains("보존"), "{msg}");
    assert_eq!(fs::read_to_string(&tools_path).unwrap(), flow);
}

#[cfg(unix)]
#[test]
fn preserves_file_mode_rejects_fifo_and_hardlink_and_recovers_stale_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let profile = init_skill(dir.path(), "hard", "demo");
    let skill_dir = profile.join("skills/demo");
    let tools_path = skill_dir.join("tools.yaml");

    // 1) 0600 권한 유지
    let base = "version: 1\nserver: demo\ntools:\n  - id: first\n    class: read\n    scopes: [demo.first]\n    input: {type: object, properties: {}, additionalProperties: false}\n";
    fs::write(&tools_path, base).unwrap();
    fs::set_permissions(&tools_path, fs::Permissions::from_mode(0o600)).unwrap();
    let ok = add_tool(&profile, &["demo.second"]);
    assert!(ok.status.success(), "{}", text(&ok));
    let mode = fs::metadata(&tools_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "기존 0600 권한을 유지해야 함(실제: {mode:o})");

    // 2) 이전 강제 종료로 남은 안전한 `.bot-add-tool.tmp`는 자동 정리 후 성공
    let stale_tmp = skill_dir.join(".bot-add-tool.tmp");
    fs::write(&stale_tmp, "partial write").unwrap();
    let recovered = add_tool(&profile, &["demo.third"]);
    assert!(recovered.status.success(), "{}", text(&recovered));
    assert!(!stale_tmp.exists(), "고아 임시 파일은 정리돼야 함");

    // 3) `.bot-add-tool.tmp`가 링크면 절대 지우지 않고 거절
    let victim = dir.path().join("victim.txt");
    fs::write(&victim, "keep").unwrap();
    std::os::unix::fs::symlink(&victim, &stale_tmp).unwrap();
    let bad_tmp = add_tool(&profile, &["demo.fourth"]);
    assert!(!bad_tmp.status.success(), "{}", text(&bad_tmp));
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
    fs::remove_file(&stale_tmp).unwrap();

    // 4) 하드링크(`nlink > 1`) 거절(연결된 바깥 파일 무손상)
    let linked_outside = dir.path().join("linked.yaml");
    fs::write(&linked_outside, base).unwrap();
    fs::remove_file(&tools_path).unwrap();
    fs::hard_link(&linked_outside, &tools_path).unwrap();
    let hard = add_tool(&profile, &["demo.fifth"]);
    assert!(!hard.status.success(), "{}", text(&hard));
    assert!(text(&hard).contains("하드링크"), "{}", text(&hard));
    assert_eq!(fs::read_to_string(&linked_outside).unwrap(), base);

    // 5) FIFO(명명 파이프)를 열 때 멈추지 않고 즉시 거절
    fs::remove_file(&tools_path).unwrap();
    let mkfifo = Command::new("mkfifo").arg(&tools_path).status().unwrap();
    assert!(mkfifo.success());
    let started = Instant::now();
    let fifo = add_tool(&profile, &["demo.sixth"]);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "FIFO에서 멈추면 안 됨: {:?}",
        started.elapsed()
    );
    assert!(!fifo.status.success(), "{}", text(&fifo));
    assert!(text(&fifo).contains("일반 파일"), "{}", text(&fifo));
}
