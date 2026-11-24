//! 도구가 받는 자격이 보관소에 없을 때(실제 `bot` 프로세스·srt 샌드박스의 합성 일정 서버·데모 모델·팀 시험 DB): `bot validate`는 엄격하게 실패하지만
//! worker는 서지 않는다 — 기동 때 이름만 경고하고, 그 도구의 호출은 브로커가 **호출을 기록하기 전에** `secret_missing`으로 거절한다(승인 카드도 열지
//! 않는다). 자격을 채우면 같은 호출이 그대로 승인 카드까지 간다. `srt`·`python3`가 없으면 건너뛴다.
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

/// 예제 프로필 복사본: 삭제 도구(`agenda.delete`, 되돌릴 수 없어 승인 카드가 열린다)가 자격 `calendar-writer`를 환경 `CALENDAR_TOKEN`으로 받는다.
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
    let (list, delete) = text.split_once("  - id: delete").expect("삭제 도구 선언");
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

/// 역할 하나를 따로 띄운다. 표준오류는 파일로 받는다.
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
            .expect("serve 시작"),
    )
}

#[test]
fn a_missing_tool_secret_fails_validate_but_never_stops_the_worker_and_the_call_is_refused_before_any_card()
 {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
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

    // validate는 엄격하다: 자격 이름과 환경 변수명을 알리고 통과하지 않는다
    let validated = bot(&refs(&env(&[])), &["validate"]);
    assert_eq!(validated.status.code(), Some(1), "{}", text(&validated));
    let message = text(&validated);
    assert!(
        message.contains("calendar-writer")
            && message.contains("BOT_SECRET__personal__calendar_writer"),
        "{message}"
    );

    // worker는 선다: 자격이 없어도 시작해 이름만 경고한다
    let without = env(&[]);
    let _gateway = start_gateway_roles(&refs(&without), "gateway", &socket);
    wait_for("활성 판본이 게시되지 않았어요", || {
        !schema.active().is_empty()
    });
    let log = dir.path().join("worker.log");
    let worker = spawn_logging(&refs(&without), "worker", &log);
    let warned = || std::fs::read_to_string(&log).unwrap_or_default();
    wait_for("worker가 도구 경고를 내지 않았어요", || {
        warned().contains("calendar-writer")
    });
    let warning = warned();
    assert!(
        warning.contains("agenda.delete") && warning.contains("사용 불가"),
        "{warning}"
    );
    // 자격이 없는 삭제 도구는 거절하지만 조회 도구는 그대로 쓴다(스킬 하나가 봇 전체를 세우지 않는다)
    let listed = bot(
        &refs(&without),
        &["chat", "--request-id", "t0", "오늘 일정 알려줘"],
    );
    assert!(listed.status.success(), "{}", text(&listed));
    assert!(text(&listed).contains("산책"), "{}", text(&listed));

    // 삭제 요청: 호출을 기록하기 전에 거절한다 — 승인 카드 0, 삭제 호출 행 0, `secret_missing` 감사 1
    // 거절이 기록 전에 일어나지 않았다면 승인 카드가 열려 run이 멈춘다: 끝나기를 기다리다 멈추지 않도록 시간을 둔다
    let refused = bot_within(
        &refs(&without),
        &["chat", "--request-id", "t1", "산책 일정을 삭제해줘"],
        std::time::Duration::from_secs(60),
    )
    .expect("삭제 요청이 끝나지 않았어요 — 거절이 승인 카드를 열기 전에 일어나야 해요");
    // 모델은 거절을 «삭제하지 않았다»로 읽고 run은 정상으로 끝난다(일정은 그대로다)
    assert!(refused.status.success(), "{}", text(&refused));
    assert!(
        text(&refused).contains("그대로 두었어요"),
        "{}",
        text(&refused)
    );
    assert_eq!(
        schema.query("select count(*) from {s}.bot_approvals"),
        "0",
        "실행기가 못 돌릴 호출로 사람에게 승인을 받으면 안 돼요"
    );
    assert_eq!(
        schema.query("select count(*) from {s}.bot_calls where tool = 'agenda.delete'"),
        "0",
        "거절한 호출은 기록하지 않는다"
    );
    assert_eq!(
        schema.query(
            "select count(*) from {s}.bot_audit where action = 'call.refuse' and target = 'agenda.delete' and verdict = 'secret_missing'"
        ),
        "1",
        "거절은 고정 단어로 감사에 남는다"
    );
    assert!(
        worker_alive(&worker),
        "자격 하나가 없다고 worker가 서면 안 돼요"
    );
    drop(worker);

    // 자격을 채우고 worker를 다시 띄우면 같은 요청이 승인 카드까지 간다(경고도 사라진다)
    let value = "value-for-the-calendar-token";
    let filled = env(&[("BOT_SECRET__personal__calendar_writer", value)]);
    let log = dir.path().join("worker2.log");
    let _worker = spawn_logging(&refs(&filled), "worker", &log);
    // 승인을 기다리며 멈추는 요청이라 `chat`은 끝나기를 기다리지 않고 DB로 본다
    let mut asking = Command::new(BOT)
        .args(["chat", "--request-id", "t2", "산책 일정을 삭제해줘"])
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME" || k == "TMPDIR"))
        .envs(refs(&filled))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("chat 시작");
    wait_for(
        "자격이 있는데도 승인 카드가 열리지 않았어요",
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

/// `bot`을 `limit` 안에 끝내면 그 출력을, 못 끝내면 죽이고 None
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
        .expect("bot 실행");
    let deadline = std::time::Instant::now() + limit;
    loop {
        if child.try_wait().expect("상태").is_some() {
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
    // 아직 끝나지 않은 프로세스(종료 상태를 읽지 않는다): 자격 점검 때문에 worker가 서 버렸다면 여기서 드러난다
    let pid = server.0.id().to_string();
    Command::new("/bin/kill")
        .args(["-0", &pid])
        .status()
        .is_ok_and(|status| status.success())
}
