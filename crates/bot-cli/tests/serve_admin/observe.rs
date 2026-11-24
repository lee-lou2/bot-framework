use super::process_support::*;

/// 수집기가 받은 요청 본문들
type Bodies = std::sync::Arc<std::sync::Mutex<Vec<Vec<u8>>>>;

/// 받은 요청 본문을 모으는 가짜 OTLP 수집기. (주소, 받은 본문들)
fn collector() -> (String, Bodies) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let (mut raw, mut chunk) = (Vec::new(), [0u8; 4096]);
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..n]);
                if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if raw.len() >= end + 4 + length {
                        sink.lock()
                            .unwrap()
                            .push(raw[end + 4..end + 4 + length].to_vec());
                        break;
                    }
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    (format!("http://127.0.0.1:{port}/v1/traces"), bodies)
}

#[test]
fn a_standalone_gateway_exports_its_spans_to_the_configured_collector() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let (endpoint, bodies) = collector();
    let host = dir.path().join("host.json");
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            "",
            &format!(
                r#", "admin": {{"bind": "127.0.0.1:0", "trace": {{"personal": {{"endpoint": "{endpoint}"}}}}}}"#
            ),
        ),
    )
    .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let url = schema.url();
    let env = env_for(
        &examples(),
        &url,
        &socket,
        &[("BOT_HOST_CONFIG", host.to_str().unwrap())],
    );
    // admin 역할 없이 gateway만 띄워도 설정한 수집기로 접수 스팬이 나간다
    let server = start_gateway(&refs(&env), &[], &socket);
    let chat = bot(
        &refs(&env),
        &["chat", "--no-wait", "--request-id", "otlp-1", "안녕"],
    );
    assert!(chat.status.success(), "{}", text(&chat));
    wait_for("수집기가 접수 스팬을 받지 못했어요", || {
        bodies
            .lock()
            .unwrap()
            .iter()
            .any(|body| body.windows(11).any(|w| w == b"ingress cli"))
    });
    drop(server);
}

/// 수집기가 받은 본문 가운데 `needle`이 든 것의 수(스팬 이름·속성 값은 본문 안에 그대로 들어 있다)
fn bodies_with(bodies: &Bodies, needle: &str) -> usize {
    bodies
        .lock()
        .unwrap()
        .iter()
        .filter(|body| body.windows(needle.len()).any(|w| w == needle.as_bytes()))
        .count()
}

/// 수집기가 받은 모든 본문에서 `needle`이 나온 횟수(한 본문에 스팬이 여럿 담기므로, 스팬 이름은 이 횟수가 스팬 수다)
fn occurrences(bodies: &Bodies, needle: &str) -> usize {
    bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| {
            body.windows(needle.len())
                .filter(|w| *w == needle.as_bytes())
                .count()
        })
        .sum()
}

/// 수집기가 «ingress cli» 스팬을 받은 횟수
fn ingress_spans(bodies: &Bodies) -> usize {
    bodies_with(bodies, "ingress cli")
}

#[test]
fn withdrawing_collection_makes_the_old_process_a_stale_one_that_exports_nothing_more() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let profiles = examples();
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let (endpoint, bodies) = collector();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let write = |name: &str, extra: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, host_json(my_uid(), &pkg, "", extra)).unwrap();
        path
    };
    // 수집을 켠 호스트 설정(내용 허용까지)과, 운영자가 수집을 거둔 같은 호스트 설정
    let collecting = write(
        "collecting.json",
        &format!(
            r#", "admin": {{"bind": "127.0.0.1:0", "trace": {{"personal": {{"endpoint": "{endpoint}", "content": true}}}}}}"#
        ),
    );
    let withdrawn = write("withdrawn.json", r#", "admin": {"bind": "127.0.0.1:0"}"#);
    let (socket_a, socket_b) = (
        dir.path().join("a").join("g.sock"),
        dir.path().join("b").join("g.sock"),
    );
    let url = schema.url();
    let env_a = env_for(
        &profiles,
        &url,
        &socket_a,
        &[("BOT_HOST_CONFIG", collecting.to_str().unwrap())],
    );
    let env_b = env_for(
        &profiles,
        &url,
        &socket_b,
        &[("BOT_HOST_CONFIG", withdrawn.to_str().unwrap())],
    );
    let a = start_gateway(&refs(&env_a), &[], &socket_a);
    wait_for(
        "첫 시작이 활성 판본을 게시하지 않았어요",
        || !schema.active().is_empty(),
    );
    let operating = schema.active();

    // 대조: 수집이 켜진 동안은 접수 스팬이 수집기에 간다
    let hello = bot(
        &refs(&env_a),
        &["chat", "--no-wait", "--request-id", "before-1", "안녕"],
    );
    assert!(hello.status.success(), "{}", text(&hello));
    wait_for("수집기가 접수 스팬을 받지 못했어요", || {
        ingress_spans(&bodies) >= 1
    });
    // 한 접수가 만든 스팬이 모두 도착해 개수가 가라앉을 때까지 기다린 뒤 기준으로 삼는다
    let mut seen = ingress_spans(&bodies);
    loop {
        std::thread::sleep(std::time::Duration::from_millis(2500));
        let now = ingress_spans(&bodies);
        if now == seen {
            break;
        }
        seen = now;
    }

    // 운영자가 수집을 거두고 명시 활성화한다: 수집 설정이 활성화 지문에 들어 있어 활성 판본이 바뀐다
    let b = start_gateway(&refs(&env_b), &["--activate"], &socket_b);
    wait_for(
        "수집을 거둔 활성화가 판본을 바꾸지 않았어요",
        || schema.active() != operating,
    );

    // 옛 설정으로 남은 A는 구판이다: 접수를 거절하고(종료 코드 3) 그 거절 스팬도, 큐에 남은 것도 옛 수집기로 보내지 않는다
    let stale = bot(
        &refs(&env_a),
        &["chat", "--no-wait", "--request-id", "after-1", "안녕"],
    );
    assert_eq!(stale.status.code(), Some(3), "{}", text(&stale));
    std::thread::sleep(std::time::Duration::from_secs(4)); // 내보내기 간격(1초)의 몇 배
    assert_eq!(
        ingress_spans(&bodies),
        seen,
        "수집을 거둔 뒤 옛 프로세스가 스팬을 더 내보냈어요"
    );
    drop((a, b));
}

const ISSUER: &str = "https://issuer.example";
const INBOX_KEY: &str = "0123456789abcdef0123456789abcdef";

/// 원시 HTTP/1.1 요청 하나를 보내고 응답 상태 코드를 돌려준다.
fn raw_request(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> u16 {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("HTTP 입구에 접속");
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut text = format!("{method} {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(text.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    String::from_utf8_lossy(&raw)
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0)
}

/// 호출자가 경로에 실은 식별자·비밀은 수집기가 받는 어떤 스팬에도 없어야 한다. 내보내기 정책은 `http.route`를 가명 처리하지
/// 않으므로(내용 기록·평문 식별자를 켠 가장 느슨한 설정에서도 값이 그대로 나간다) 게이트웨이가 요청이 보낸 경로를 기록하지
/// 않는 것이 유일한 방어선이다. 실제 게이트웨이 프로세스 → 내보내기 → 수집기 전 구간을 본다.
#[test]
fn identifiers_and_secrets_carried_in_a_request_path_never_reach_the_collector() {
    let Some(schema) = Schema::create() else {
        eprintln!("BOT_TEST_DATABASE_URL이 없어 건너뜀");
        return;
    };
    let dir = tempfile::Builder::new()
        .prefix("g")
        .tempdir_in("/tmp")
        .unwrap();
    let profiles = dir.path().join("profiles");
    with_inbox(&profiles);
    let (endpoint, bodies) = collector();
    // 닫힌 임시 포트를 입구 주소로 쓴다(게이트웨이는 설정한 주소를 그대로 연다)
    let port = free_port();
    let pkg = package(dir.path(), "#!/bin/sh\nexit 0\n");
    let host = dir.path().join("host.json");
    std::fs::write(
        &host,
        host_json(
            my_uid(),
            &pkg,
            &format!(
                r#", "http": {{"listen": "127.0.0.1:{port}", "endpoints": {{"inbox": {{"issuer": "{ISSUER}", "audience": "bot", "key": "inbox-key"}}}}}}"#
            ),
            &format!(
                r#", "admin": {{"bind": "127.0.0.1:0", "trace": {{"personal": {{"endpoint": "{endpoint}", "content": true, "ids": "plain"}}}}}}"#
            ),
        ),
    )
    .unwrap();
    let socket = dir.path().join("run").join("g.sock");
    let url = schema.url();
    let env = env_for(
        &profiles,
        &url,
        &socket,
        &[
            ("BOT_HOST_CONFIG", host.to_str().unwrap()),
            ("BOT_SECRET__personal__inbox_key", INBOX_KEY),
        ],
    );
    let server = start_gateway(&refs(&env), &[], &socket);
    wait_for(
        "첫 시작이 활성 판본을 게시하지 않았어요",
        || !schema.active().is_empty(),
    );
    wait_for("HTTP 입구가 열리지 않았어요", || {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
    });

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let token = bot_gateway::http::token::sign(
        &serde_json::json!({"iss": ISSUER, "aud": "bot", "sub": "svc", "exp": now + 3600}),
        INBOX_KEY.as_bytes(),
    );
    let auth = format!("Bearer {token}");
    let send = |method: &str, path: &str, key: &str, body: &str| {
        raw_request(
            port,
            method,
            path,
            &[
                ("Authorization", &auth),
                ("Idempotency-Key", key),
                ("Content-Type", "application/json"),
            ],
            body,
        )
    };

    // 대조: 등록된 트리거와 맞는 요청은 접수된다(인증·권한·경로가 모두 실제로 통과한다)
    assert_eq!(send("POST", "/inbox/notify", "control-1", "{}"), 202);

    // 호출자가 경로에 식별자·비밀을 실은 요청들: 등록된 트리거와 맞지 않아 모두 404지만 접수 스팬은 남는다
    const ID: &str = "usr-4242-ZQJXKW";
    const SECRET: &str = "sk-live-Qm9x7vT2pLdR";
    let hostile = [
        ("POST", format!("/inbox/users/{ID}/keys/{SECRET}")),
        ("PUT", "/inbox/notify".to_owned()),
        ("POST", format!("/inbox/Notify/{SECRET}")),
        ("POST", format!("/inbox/notify%2F{ID}%2F{SECRET}")),
        ("GET", format!("/inbox/notify/{ID}?token={SECRET}")),
    ];
    for (n, (method, path)) in hostile.iter().enumerate() {
        assert_eq!(
            send(method, path, &format!("hostile-{n}"), ""),
            404,
            "{method} {path}"
        );
    }

    // 모든 접수 스팬(대조 1 + 적대적 5)이 수집기에 도착할 때까지 기다린다 — 그 전에 «없다»를 확인하면 아무것도 보지 않은 셈이다
    let expected = 1 + hostile.len();
    wait_for(
        "수집기가 접수 스팬을 모두 받지 못했어요",
        || occurrences(&bodies, "ingress http") >= expected,
    );
    std::thread::sleep(std::time::Duration::from_millis(2500)); // 늦게 오는 배치까지(내보내기 간격 1초)

    // 요청이 보낸 경로·식별자·비밀은 어떤 본문에도 없다
    for needle in [
        ID,
        SECRET,
        "ZQJXKW",
        "Qm9x7vT2pLdR",
        "usr-4242",
        "sk-live",
        "/keys/",
        "Notify",
    ] {
        assert_eq!(
            bodies_with(&bodies, needle),
            0,
            "경로에 실은 «{needle}»이 수집기에 갔어요"
        );
    }
    // 대신 고정 값이 있고(경로를 «기록하지 않음»이 아니라 «가려서 기록함»), 맞은 요청은 등록된 정의의 값을 쓴다
    assert!(
        bodies_with(&bodies, "unmatched") >= 1,
        "맞지 않은 요청의 고정 경로 값이 없어요"
    );
    assert!(
        bodies_with(&bodies, "_OTHER") >= 1,
        "맞지 않은 요청의 고정 method 값이 없어요"
    );
    assert!(
        bodies_with(&bodies, "/notify") >= 1,
        "맞은 요청의 정의 경로 값이 없어요"
    );
    drop(server);
}
