//! Direct MCP protocol tests for `examples/servers/demo-agenda` against the example `tools.yaml` contract.

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
    /// When `clean_env` is true, clears the environment (keeping only `PATH` and `LC_ALL=C`) to emulate sandboxed per-call execution.
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
        let mut child = command.spawn().expect("failed to start demo-agenda server");
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
        stdin.write_all(text.as_bytes()).expect("write");
        stdin.write_all(b"\n").expect("write");
        stdin.flush().expect("flush");
    }

    fn recv(&self) -> Value {
        let line = self
            .lines
            .recv_timeout(Duration::from_secs(10))
            .expect("no response within 10s");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid JSON ({e}): {line}"))
    }

    fn ask(&mut self, method: &str, params: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send_raw(
            &json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string(),
        );
        let answer = self.recv();
        assert_eq!(answer["id"], id, "response id mismatch: {answer}");
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

fn payload(result: &Value) -> Value {
    let text = result["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("expected text content: {result}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("content is not JSON ({e}): {text}"))
}

#[test]
fn tool_names_and_input_schemas_equal_the_example_tools_yaml() {
    if !python_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    let set = load(
        &examples().join("profiles"),
        &HostCatalog::demo(),
        &Limits::default(),
    )
    .unwrap_or_else(|r| panic!("example profile validation failed:\n{r}"));
    let snap = set.profile(&ProfileId("personal".into())).unwrap();
    let agenda = &snap.agents[&AgentId("main".into())].skills["agenda"];
    let declared: std::collections::BTreeMap<String, Value> = agenda
        .tools
        .iter()
        .map(|(id, tool)| {
            let name =
                id.0.strip_prefix("agenda.")
                    .expect("agenda.<tool>")
                    .to_owned();
            (
                name,
                serde_json::from_str(&tool.input.0).expect("input schema JSON"),
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
        .expect("tools array")
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
        "advertised tools must match declared tools"
    );
    for (name, schema) in &declared {
        assert_eq!(
            &advertised[name], schema,
            "input schema mismatch for {name}"
        );
    }
}

#[test]
fn delete_is_a_simulation_and_nothing_is_stored() {
    if !python_available() {
        eprintln!("skipping: python3 unavailable");
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
            .any(|e| e["id"] == "e-walk" && e["title"] == "Walk"),
        "{before}"
    );
    assert_eq!(before["simulated"], true);

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
        (Some("e-walk"), Some("Walk"))
    );
    let id = receipt["receipt"].as_str().expect("receipt ID");
    assert!(id.starts_with("sim-") && id.len() == 16, "{id}");
    let message = receipt["message"].as_str().unwrap();
    let lower = message.to_lowercase();
    assert!(
        (lower.contains("simulat") || lower.contains("virtual"))
            && lower.contains("receipt")
            && (lower.contains("not permanently")
                || lower.contains("not deleted")
                || lower.contains("not actually")),
        "{message}"
    );

    let again = payload(&server.call("delete", json!({"event": "e-walk"}), Some("c-17")));
    assert_eq!(again["receipt"], receipt["receipt"]);
    let other = payload(&server.call("delete", json!({"event": "e-walk"}), Some("c-18")));
    assert_ne!(other["receipt"], receipt["receipt"]);

    // Logical call identity is (profile-id, run-id, call-id); generation changes on retries/handovers and is excluded.
    let walk = json!({"event": "e-walk"});
    let first = ("personal", "r-1", "cli-1-0", 1);
    let base = payload(&server.call_logical(walk.clone(), first))["receipt"].clone();
    assert!(base.as_str().is_some_and(|r| r.starts_with("sim-")));
    for (same, why) in [
        (("personal", "r-1", "cli-1-0", 1), "identical replay"),
        (
            ("personal", "r-1", "cli-1-0", 7),
            "retry/handover generation",
        ),
    ] {
        let again = payload(&server.call_logical(walk.clone(), same))["receipt"].clone();
        assert_eq!(again, base, "{why} must keep the same receipt");
    }
    for (different, why) in [
        (("personal", "r-2", "cli-1-0", 1), "different run"),
        (("other", "r-1", "cli-1-0", 1), "different profile"),
        (("personal", "r-1", "cli-1-1", 1), "next call in same run"),
    ] {
        let other = payload(&server.call_logical(walk.clone(), different))["receipt"].clone();
        assert_ne!(other, base, "{why} must produce a distinct receipt");
    }
    let other_event =
        payload(&server.call_logical(json!({"event": "e-meet"}), first))["receipt"].clone();
    assert_ne!(other_event, base);

    assert_eq!(payload(&server.call("list", json!({}), None)), before);

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
        "arguments must be an object"
    );

    assert_eq!(
        server.call("no-such-tool", json!({}), None)["error"]["code"],
        -32602
    );
    assert_eq!(
        server.ask("resources/list", json!({}))["error"]["code"],
        -32601
    );
    assert_eq!(server.ask("ping", json!({}))["result"], json!({}));

    // Server stays responsive after malformed or oversized lines.
    server.send_raw("{not json");
    assert_eq!(server.recv()["error"]["code"], -32700);
    server.send_raw(&"x".repeat(1_500_000));
    assert_eq!(server.recv()["error"]["code"], -32600);
    assert_eq!(server.ask("ping", json!({}))["result"], json!({}));

    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "server must not write any files into the workspace"
    );
}

#[test]
fn is_an_executable_standalone_script_with_a_usage_message() {
    let path = server_path();
    let mode = std::fs::metadata(&path)
        .expect("server file")
        .permissions()
        .mode();
    assert!(
        mode & 0o111 != 0,
        "server script must be executable: {mode:o}"
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
            "demo server must not use network, env, or file I/O: {banned}"
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
        .expect("spawn server");
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("--stdio"));
    let help = Command::new(&path)
        .arg("--help")
        .current_dir(dir.path())
        .output()
        .expect("spawn server");
    assert_eq!(help.status.code(), Some(0));
}
