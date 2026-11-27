//! `bot doctor`를 실제 `bot` 프로세스로 확인한다: 영역별 한국어 요약, 기본은 네트워크에 접속하지 않음(연결 시도가 없음을 열린 포트로 확인),
//! 자격은 이름만 보이고 값·접속 주소는 어디에도 나오지 않음, 없는 DB·깨진 호스트 설정은 종료 코드 1과 안내. 시험 DB가 필요 없다.
use crate::process_support::*;
use std::{fs, net::TcpListener, path::PathBuf};

const PASSWORD: &str = "pw-SENTINEL-4f1c9";
const SECRET: &str = "value-SENTINEL-7a2e0";

struct Work {
    dir: tempfile::TempDir,
}

impl Work {
    /// `bot init`으로 만든 작업 폴더(프로필 `demo`)
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
    // 아무도 받지 않는 주소가 아니라, 연결이 오면 알 수 있는 열린 포트에 DB 주소를 둔다
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "postgres://user:{PASSWORD}@127.0.0.1:{}/db",
        listener.local_addr().unwrap().port()
    );
    let out = work.doctor(&[("BOT_DATABASE_URL", url.as_str())], &[]);
    let shown = text(&out);
    assert!(out.status.success(), "{shown}");
    for line in [
        "[통과] 호스트 설정",
        "[통과] 모델 제공자",
        "[통과] 도구 서버",
        "[통과] 프로필 — 1개 검증을 통과했어요",
        "[통과] 입구",
        "[통과] 자격 — 필요한 자격이 없어요",
        "[통과] 데이터베이스 — 주소가 설정돼 있어요",
        "[미검사] 데이터베이스 연결",
        "요약: 통과 7 · 주의 0 · 실패 0 · 미검사 1",
    ] {
        assert!(shown.contains(line), "{line}\n{shown}");
    }
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "기본 점검은 접속하지 않는다"
    );
    for hidden in [PASSWORD, "127.0.0.1"] {
        assert!(!shown.contains(hidden), "주소는 출력하지 않는다: {shown}");
    }
    // 같은 입력은 같은 출력이다
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
        stdout.contains("[실패] 데이터베이스 — BOT_DATABASE_URL이 없어요"),
        "{stdout}"
    );
    assert!(stdout.contains("[통과] 프로필"), "{stdout}");
    assert!(
        stdout.contains("[미검사] 데이터베이스 연결 — 주소가 없어"),
        "{stdout}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("bot: 점검에서 실패한 항목이 1개예요"));
    // 형식이 틀린 주소도 값을 보여 주지 않고 실패한다
    let bad = format!("mysql://u:{PASSWORD}@h/db");
    let out = work.doctor(&[("BOT_DATABASE_URL", bad.as_str())], &[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out).contains("형식이 맞지 않아요") && !text(&out).contains(PASSWORD));
}

#[test]
fn check_db_reports_a_failed_connection_without_printing_the_address() {
    let work = Work::new("personal");
    // 받자마자 끊는 서버: 접속 거부와 달리 연결 라이브러리가 다시 시도하지 않아 바로 실패한다
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || listener.incoming().for_each(drop));
    let url = format!("postgres://user:{PASSWORD}@127.0.0.1:{port}/db");
    let out = work.doctor(&[("BOT_DATABASE_URL", url.as_str())], &["--check-db"]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(
        shown.contains("[실패] 데이터베이스 연결 — 연결하지 못했어요"),
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
    assert!(shown.contains("[실패] 호스트 설정"), "{shown}");
    assert!(shown.contains("JSON 객체여야 해요"), "{shown}");
    assert!(
        shown.contains("[미검사] 프로필 — 호스트 설정이 올바르지 않아"),
        "{shown}"
    );
    assert!(shown.contains("[미검사] 자격"), "{shown}");
    // 없는 파일·알 수 없는 키도 같다. --host가 BOT_HOST_CONFIG보다 앞선다
    let gone = work.dir.path().join("gone.json");
    let out = work.doctor(&[], &["--host", gone.to_str().unwrap()]);
    assert!(
        text(&out).contains("호스트 설정 파일을 읽지 못했어요"),
        "{}",
        text(&out)
    );
    fs::write(work.host(), r#"{"version": 1, "surprise": true}"#).unwrap();
    assert!(text(&work.doctor(&[], &[])).contains("알 수 없는 최상위 키"));
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
    assert!(shown.contains("[실패] 자격"), "{shown}");
    assert!(
        shown.contains("beta-key"),
        "없는 자격은 이름으로 알린다: {shown}"
    );
    assert!(shown.contains("BOT_SECRET__demo__beta_key"), "{shown}");
    assert!(
        !shown.contains("alpha-key"),
        "있는 자격은 나오지 않는다: {shown}"
    );
    assert!(!shown.contains(SECRET), "값은 출력하지 않는다: {shown}");
    // 다 갖추면 통과하고 그래도 값은 나오지 않는다
    let out = work.doctor(
        &[
            ("BOT_DATABASE_URL", "postgres://u@localhost/db"),
            ("BOT_SECRET__demo__alpha_key", SECRET),
            ("BOT_SECRET__demo__beta_key", SECRET),
        ],
        &[],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("[통과] 자격 — 필요한 자격 2개가 모두 있어요"));
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
    // 쿼리가 든 주소는 호스트 설정 단계에서 거절된다(자격·쿼리 금지)
    fs::write(work.host(), &host).unwrap();
    let out = work.doctor(&[("BOT_DATABASE_URL", "postgres://u@localhost/db")], &[]);
    assert!(text(&out).contains("[실패] 호스트 설정"), "{}", text(&out));
    fs::write(work.host(), host.replace("/v1?k=x", "/v1")).unwrap();
    let out = work.doctor(&[("BOT_DATABASE_URL", "postgres://u@localhost/db")], &[]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    for expected in [
        "[실패] 모델 제공자",
        "ghost의 실행 방법이 없어요",
        "sandbox.apple",
        "real: claude-cli · 모델 synthetic-model · 상류 api.example.test · 자격 이름 model-key",
        "[실패] 자격",
        "model-key",
    ] {
        assert!(shown.contains(expected), "{expected}\n{shown}");
    }
    assert!(
        !shown.contains("/v1"),
        "상류 경로는 보여 주지 않는다: {shown}"
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
        shown
            .contains("[주의] 호스트 설정 — BOT_HOST_CONFIG·--host가 없어 내장 데모 호스트를 써요"),
        "{shown}"
    );
    assert!(
        shown.contains("요약: 통과 6 · 주의 1 · 실패 0 · 미검사 1"),
        "{shown}"
    );
}

/// 막음 3: 주소의 모르는 매개변수 값(합성 `sslpassword`)이 드라이버 경고로 새지 않는다(접속하지 않을 때도, 접속할 때도).
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

/// 막음 4: 접속하지 않는 기본 점검도 연결 라이브러리와 같은 구문 분석으로 주소를 본다(고정 문구만 나오고 주소는 안 나온다).
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
            shown.contains("[실패] 데이터베이스 — BOT_DATABASE_URL 형식이 맞지 않아요"),
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

/// 막음 5: 수집(OTLP) 헤더가 참조하는 자격도 다른 자격처럼 점검한다 — `bot validate`·`serve`가 같은 목록을 쓴다.
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
    assert!(shown.contains("[실패] 자격"), "{shown}");
    assert!(shown.contains("collector-token"), "{shown}");
    let (profiles, host) = (work.profiles(), work.host());
    let checked = bot(
        &[
            ("BOT_PROFILES", profiles.to_str().unwrap()),
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
        ],
        &["validate"],
    );
    assert!(!checked.status.success(), "validate도 같은 목록을 쓴다");
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

/// 막음 6: worker가 거절하는 `state: required` 도구 서버를 doctor도 실패로 본다(같은 판정 함수). `ephemeral`이면 같은 서버가 통과한다.
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
            assert!(shown.contains("[통과] 도구 서버"), "{shown}");
        } else {
            assert!(shown.contains("[실패] 도구 서버"), "{shown}");
            assert!(shown.contains("servers.stateful.state=required"), "{shown}");
        }
    }
}

/// 작업 1(API 직접 호출) 연결: 자격 이름·상류 호스트만 보이고, 주소·키 규칙은 worker 시작이 쓰는 함수가 판정한다.
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
    assert_eq!(out.status.code(), Some(1), "키가 없다: {shown}");
    for expected in [
        "[통과] 모델 제공자 — 5개",
        "claude: anthropic-api · 모델 synthetic-model · 상류 api.example.test · 자격 이름 anthropic-key",
        "gpt: openai-api · 모델 synthetic-model · 상류 api.example.test · 자격 이름 openai-key · 최대 토큰 1024",
        "local: openai-api · 모델 synthetic-model · 상류 127.0.0.1:11434 · 서비스 키 없음(loopback 전용)",
        "gpt-new: openai-api · 모델 synthetic-model · 상류 api.example.test · 자격 이름 openai-key · 최대 토큰 2048(max_completion_tokens)",
        "[실패] 자격",
        "anthropic-key",
        "openai-key",
    ] {
        assert!(shown.contains(expected), "{expected}\n{shown}");
    }
    assert!(
        !shown.contains("/private/path"),
        "상류 경로는 보이지 않는다: {shown}"
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
    // 키 없는 주소는 loopback뿐이다: worker 시작과 같은 규칙으로 거절된다
    with_host_keys(&work, |host| {
        host["providers"]["local"]["endpoint"] = "https://api.example.test/v1".into();
    });
    let out = work.doctor(&[db], &[]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("loopback"), "{}", text(&out));
}

/// 작업 2(원격 MCP 서버) 연결: 판본은 로컬·원격 모두 `measure_all`로, IP 리터럴 정책은 연결 때 쓰는 함수로 거른다. 접속하지 않는다.
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
        "[통과] 도구 서버 — 2개",
        "notes: 원격 https · tools.example.test:443 · bearer 자격 이름 notes-token · 이름 풀이는 접속할 때 검사해요",
        "local-mcp: 원격 http · 127.0.0.1:8931 · 인증 없음",
    ] {
        assert!(shown.contains(expected), "{expected}\n{shown}");
    }
    assert!(
        !shown.contains("secret-in-path"),
        "경로·질의는 보이지 않는다: {shown}"
    );
    // 사설 IP 리터럴은 연결 때 거절되므로 미리 실패로 알린다
    with_host_keys(&work, |host| {
        host["servers"]["intranet"] =
            serde_json::json!({"transport": "http", "url": "https://10.1.2.3/mcp"});
    });
    let out = work.doctor(&env, &[]);
    let shown = text(&out);
    assert_eq!(out.status.code(), Some(1), "{shown}");
    assert!(shown.contains("[실패] 도구 서버"), "{shown}");
    assert!(shown.contains("servers.intranet"), "{shown}");
}
