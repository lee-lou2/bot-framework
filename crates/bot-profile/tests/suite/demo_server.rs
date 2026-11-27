//! 예시 도구 서버(`examples/servers/demo-agenda`)를 MCP로 직접 주고받아 확인한다:
//! 도구 이름·입력 스키마가 예시 프로필의 tools.yaml과 같고, `delete`는 합성 ID만 받아 가상 영수증을 돌려주며
//! 아무것도 저장하지 않는다. python3가 없으면 건너뛴다(공개 CI에는 있다).

use crate::support::Temp;
use bot_core::id::{AgentId, ProfileId};
use bot_profile::{HostCatalog, Limits, load};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

fn server_path() -> PathBuf {
    examples().join("servers/demo-agenda/bin/server")
}

fn python_available() -> bool {
    Command::new("python3").arg("--version").output().is_ok()
}

struct Server {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
    next_id: u64,
}

impl Server {
    /// `clean_env`: 환경을 비우고(PATH만 남김) C 로캘로 돌린다 — 호출별 빈 환경을 흉내 낸다.
    fn start(dir: &Path, clean_env: bool) -> Self {
        let mut command = Command::new(server_path());
        command
            .arg("--stdio")
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if clean_env {
            command
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap_or_default())
                .env("LC_ALL", "C");
        }
        let mut child = command.spawn().expect("서버를 시작하지 못했어요");
        let out = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
            next_id: 0,
        }
    }

    fn send_raw(&mut self, text: &str) {
        let stdin = self.stdin.as_mut().expect("stdin");
        stdin.write_all(text.as_bytes()).expect("쓰기");
        stdin.write_all(b"\n").expect("쓰기");
        stdin.flush().expect("flush");
    }

    fn recv(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(10))
            .expect("10초 안에 답이 없어요");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("JSON이 아니에요({e}): {line}"))
    }

    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send_raw(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        let answer = self.recv();
        assert_eq!(answer["id"], id, "요청과 같은 id로 답해야 해요: {answer}");
        assert_eq!(answer["jsonrpc"], "2.0");
        answer
    }

    fn call(&mut self, name: &str, arguments: Value, call_id: Option<&str>) -> Value {
        let mut params = json!({"name": name, "arguments": arguments});
        if let Some(id) = call_id {
            params["_meta"] = json!({"bot/call-id": id, "bot/run-id": "r-1", "bot/generation": 1});
        }
        self.ask("tools/call", params)
    }

    /// 호스트가 넣는 `_meta` 전부(profile-id·run-id·call-id·generation)로 부른다.
    fn call_logical(
        &mut self,
        arguments: Value,
        (profile, run, call, generation): (&str, &str, &str, u64),
    ) -> Value {
        let params = json!({
            "name": "delete",
            "arguments": arguments,
            "_meta": {
                "bot/profile-id": profile,
                "bot/run-id": run,
                "bot/call-id": call,
                "bot/generation": generation,
            },
        });
        self.ask("tools/call", params)
    }

    fn handshake(&mut self) -> Value {
        let init = self.ask(
            "initialize",
            json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
        );
        self.send_raw(
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string(),
        );
        init
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        drop(self.stdin.take());
        for _ in 0..50 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// 결과의 첫 글 조각을 JSON으로 읽는다.
fn payload(result: &Value) -> Value {
    let text = result["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("글 결과가 아니에요: {result}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("결과 글이 JSON이 아니에요({e}): {text}"))
}

#[test]
fn tool_names_and_input_schemas_equal_the_example_tools_yaml() {
    if !python_available() {
        eprintln!("python3가 없어 건너뜀");
        return;
    }
    let set = load(
        &examples().join("profiles"),
        &HostCatalog::demo(),
        &Limits::default(),
    )
    .unwrap_or_else(|r| panic!("예시 프로필 검증 실패:\n{r}"));
    let snap = set.profile(&ProfileId("personal".into())).unwrap();
    let agenda = &snap.agents[&AgentId("main".into())].skills["agenda"];
    let declared: std::collections::BTreeMap<String, Value> = agenda
        .tools
        .iter()
        .map(|(id, tool)| {
            let name =
                id.0.strip_prefix("agenda.")
                    .expect("agenda.<도구>")
                    .to_owned();
            (
                name,
                serde_json::from_str(&tool.input.0).expect("입력 스키마 JSON"),
            )
        })
        .collect();

    let dir = Temp::new("demo-schema");
    let mut server = Server::start(dir.path(), false);
    let init = server.handshake();
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "demo-agenda");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    let listed = server.ask("tools/list", json!({}));
    let advertised: std::collections::BTreeMap<String, Value> = listed["result"]["tools"]
        .as_array()
        .expect("tools 배열")
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap().to_owned(),
                t["inputSchema"].clone(),
            )
        })
        .collect();
    assert_eq!(
        advertised.keys().collect::<Vec<_>>(),
        declared.keys().collect::<Vec<_>>(),
        "서버가 광고하는 도구와 선언한 도구가 같아야 해요"
    );
    for (name, schema) in &declared {
        assert_eq!(
            &advertised[name], schema,
            "{name}의 입력 스키마가 tools.yaml과 달라요"
        );
    }
}

#[test]
fn delete_is_a_simulation_and_nothing_is_stored() {
    if !python_available() {
        eprintln!("python3가 없어 건너뜀");
        return;
    }
    let dir = Temp::new("demo-simulation");
    let mut server = Server::start(dir.path(), true);
    server.handshake();

    let before = payload(&server.call("list", json!({}), None));
    let events = before["events"].as_array().expect("events");
    assert!(
        events
            .iter()
            .any(|e| e["id"] == "e-walk" && e["title"] == "산책"),
        "{before}"
    );
    assert_eq!(before["simulated"], true);

    // 합성 ID 하나: 가상 영수증, 영구 삭제라고 하지 않는다
    let first = server.call("delete", json!({"event": "e-walk"}), Some("c-17"));
    assert_eq!(first["result"]["isError"], false, "{first}");
    let receipt = payload(&first);
    assert_eq!(
        (
            receipt["simulated"].as_bool(),
            receipt["persisted"].as_bool()
        ),
        (Some(true), Some(false))
    );
    assert_eq!(
        (receipt["event"].as_str(), receipt["title"].as_str()),
        (Some("e-walk"), Some("산책"))
    );
    let id = receipt["receipt"].as_str().expect("영수증 ID");
    assert!(id.starts_with("sim-") && id.len() == 16, "{id}");
    let message = receipt["message"].as_str().unwrap();
    assert!(
        message.contains("가상 삭제 영수증") && message.contains("영구 삭제 아님"),
        "{message}"
    );

    // 같은 호출 ID는 같은 영수증, 다른 호출 ID는 다른 영수증
    let again = payload(&server.call("delete", json!({"event": "e-walk"}), Some("c-17")));
    assert_eq!(again["receipt"], receipt["receipt"]);
    let other = payload(&server.call("delete", json!({"event": "e-walk"}), Some("c-18")));
    assert_ne!(other["receipt"], receipt["receipt"]);

    // 논리 호출 = profile-id·run-id·call-id 셋. 세대는 재시도·인계마다 바뀌므로 영수증에 영향이 없고,
    // call-id(run 안의 위치)가 같아도 run이나 profile이 다르면 영수증이 구별된다
    let walk = json!({"event": "e-walk"});
    let first = ("personal", "r-1", "cli-1-0", 1);
    let base = payload(&server.call_logical(walk.clone(), first))["receipt"].clone();
    assert!(base.as_str().is_some_and(|r| r.starts_with("sim-")));
    for (same, why) in [
        (("personal", "r-1", "cli-1-0", 1), "같은 호출 재전송"),
        (("personal", "r-1", "cli-1-0", 7), "세대가 다른 인계·재시도"),
    ] {
        let again = payload(&server.call_logical(walk.clone(), same))["receipt"].clone();
        assert_eq!(again, base, "{why}는 같은 영수증이어야 해요");
    }
    for (different, why) in [
        (("personal", "r-2", "cli-1-0", 1), "다른 run의 첫 호출"),
        (("other", "r-1", "cli-1-0", 1), "다른 프로필"),
        (("personal", "r-1", "cli-1-1", 1), "같은 run의 다음 호출"),
    ] {
        let other = payload(&server.call_logical(walk.clone(), different))["receipt"].clone();
        assert_ne!(other, base, "{why}는 구별되어야 해요");
    }
    // 같은 논리 호출이라도 지우는 대상이 다르면 다른 영수증
    let other_event =
        payload(&server.call_logical(json!({"event": "e-meet"}), first))["receipt"].clone();
    assert_ne!(other_event, base);

    // 지운 척했어도 목록은 그대로(저장하지 않는다)
    assert_eq!(payload(&server.call("list", json!({}), None)), before);

    // 합성 ID가 아니거나 인자가 틀리면 도구 오류
    for (name, args) in [
        ("delete", json!({"event": "nope"})),
        ("delete", json!({"event": 5})),
        ("delete", json!({})),
        ("delete", json!({"event": "e-walk", "extra": true})),
        ("list", json!({"x": 1})),
    ] {
        let answer = server.call(name, args.clone(), None);
        assert_eq!(answer["result"]["isError"], true, "{name} {args}: {answer}");
    }
    assert_eq!(
        payload(&server.call("delete", json!({"event": "nope"}), None))["error"],
        "unknown-event"
    );
    assert_eq!(
        server.call("list", json!([]), None)["result"]["isError"],
        true,
        "arguments는 객체여야 해요"
    );

    // 프로토콜 오류
    assert_eq!(
        server.call("no-such-tool", json!({}), None)["error"]["code"],
        -32602
    );
    assert_eq!(
        server.ask("resources/list", json!({}))["error"]["code"],
        -32601
    );
    assert_eq!(server.ask("ping", json!({}))["result"], json!({}));

    // 깨진 줄·너무 긴 줄 뒤에도 서버는 계속 답한다
    server.send_raw("{not json");
    assert_eq!(server.recv()["error"]["code"], -32700);
    server.send_raw(&"x".repeat(1_500_000));
    assert_eq!(server.recv()["error"]["code"], -32600);
    assert_eq!(server.ask("ping", json!({}))["result"], json!({}));

    // 호출별 빈 작업 공간: 아무 파일도 만들지 않았다
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "작업 공간에 흔적을 남기면 안 돼요"
    );
}

#[test]
fn is_an_executable_standalone_script_with_a_usage_message() {
    let path = server_path();
    let mode = std::fs::metadata(&path)
        .expect("서버 파일")
        .permissions()
        .mode();
    assert!(
        mode & 0o111 != 0,
        "실행 비트가 있어야 해요(설치본 command[0]): {mode:o}"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("#!/usr/bin/env python3\n"));
    for banned in [
        "import socket",
        "import urllib",
        "import http",
        "import requests",
        "os.environ",
        "open(",
    ] {
        assert!(
            !text.contains(banned),
            "외부 접속·환경·파일 접근이 없어야 해요: {banned}"
        );
    }
    if !python_available() {
        return;
    }
    let dir = Temp::new("demo-usage");
    let bad = Command::new(&path)
        .arg("--bogus")
        .current_dir(dir.path())
        .output()
        .expect("실행");
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("--stdio"));
    let help = Command::new(&path)
        .arg("--help")
        .current_dir(dir.path())
        .output()
        .expect("실행");
    assert_eq!(help.status.code(), Some(0));
}
