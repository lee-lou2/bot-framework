//! Shared doubles for `skill_authoring` end-to-end tests: fake OpenAI upstream LLM, fake `sops`,
//! and a minimal OpenAI HTTP client.
#![allow(dead_code)]

use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    fs,
    io::{Read, Write},
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

pub enum Turn {
    /// Invokes the first advertised tool alias starting with `prefix`.
    CallTool {
        prefix: String,
        args: Value,
    },
    Reply(String),
}

pub struct RecordedRequest {
    pub system: String,
    pub tool_names: Vec<String>,
    pub messages: Vec<Value>,
}

pub struct FakeLlm {
    port: u16,
    script: Arc<Mutex<VecDeque<Turn>>>,
    seen: Arc<Mutex<Vec<RecordedRequest>>>,
    task: JoinHandle<()>,
}

impl Drop for FakeLlm {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeLlm {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let script = Arc::new(Mutex::new(VecDeque::<Turn>::new()));
        let seen = Arc::new(Mutex::new(Vec::<RecordedRequest>::new()));
        let s = script.clone();
        let r = seen.clone();
        let task = tokio::spawn(async move {
            let mut seq = 0u64;
            while let Ok((mut stream, _)) = listener.accept().await {
                seq += 1;
                let s = s.clone();
                let r = r.clone();
                tokio::spawn(async move {
                    let _ = handle_llm(&mut stream, &s, &r, seq).await;
                });
            }
        });
        Self {
            port,
            script,
            seen,
            task,
        }
    }

    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    pub fn push(&self, turns: impl IntoIterator<Item = Turn>) {
        self.script.lock().unwrap().extend(turns);
    }

    pub fn take_seen(&self) -> Vec<RecordedRequest> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

async fn handle_llm(
    stream: &mut tokio::net::TcpStream,
    script: &Mutex<VecDeque<Turn>>,
    seen: &Mutex<Vec<RecordedRequest>>,
    seq: u64,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]);
    let content_len = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < content_len {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    let req: Value = serde_json::from_slice(&body).expect("LLM request must be JSON");
    let messages = req
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let system = messages
        .iter()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("system"))
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let tool_names: Vec<String> = req
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.pointer("/function/name").and_then(Value::as_str))
        .map(str::to_owned)
        .collect();
    seen.lock().unwrap().push(RecordedRequest {
        system,
        tool_names: tool_names.clone(),
        messages,
    });

    let turn = script
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or_else(|| Turn::Reply("done".into()));
    let response = match turn {
        Turn::CallTool { prefix, args } => {
            let alias = tool_names
                .iter()
                .find(|name| name.starts_with(&prefix))
                .cloned()
                .unwrap_or_else(|| {
                    panic!("no tool matching `{prefix}*` in request tools: {tool_names:?}")
                });
            json!({
                "id": format!("chatcmpl-{seq}"),
                "object": "chat.completion",
                "model": "fake-llm",
                "choices": [{
                    "index": 0,
                    "finish_reason": "tool_calls",
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "tool_calls": [{
                            "id": format!("call_{seq}"),
                            "type": "function",
                            "function": {
                                "name": alias,
                                "arguments": args.to_string()
                            }
                        }]
                    }
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 5}
            })
        }
        Turn::Reply(content) => json!({
            "id": format!("chatcmpl-{seq}"),
            "object": "chat.completion",
            "model": "fake-llm",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": content}
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5}
        }),
    };
    let payload = response.to_string();
    let reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    );
    stream.write_all(reply.as_bytes()).await?;
    stream.flush().await
}

/// Writes an executable fake `sops` script supporting `--decrypt`, `set`, and `unset` on JSON files.
pub fn write_fake_sops(dir: &Path) -> PathBuf {
    let path = dir.join("fake-sops");
    let script = r#"#!/usr/bin/env python3
import json, re, sys

args = sys.argv[1:]
if args[:3] == ["--decrypt", "--output-type", "json"]:
    with open(args[3], "r", encoding="utf-8") as f:
        print(f.read())
    sys.exit(0)
if args[0] == "set":
    keys = re.findall(r'\["([^"]+)"\]', args[1])
    val = json.loads(args[2])
    path = args[3]
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
    cur = data
    for k in keys[:-1]:
        cur = cur[k]
    cur[keys[-1]] = val
    with open(path, "w", encoding="utf-8") as f:
        json.dump(data, f)
    sys.exit(0)
if args[0] == "unset":
    keys = re.findall(r'\["([^"]+)"\]', args[1])
    path = args[2]
    with open(path, "r", encoding="utf-8") as f:
        data = json.load(f)
    cur = data
    for k in keys[:-1]:
        cur = cur[k]
    cur.pop(keys[-1], None)
    with open(path, "w", encoding="utf-8") as f:
        json.dump(data, f)
    sys.exit(0)
sys.exit(2)
"#;
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

pub async fn chat(port: u16, key: &str, model: &str, prompt: &str) -> (u16, String) {
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
    })
    .to_string();
    let addr = format!("127.0.0.1:{port}");
    let key = key.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(&addr).expect("connect to OpenAI entrance");
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let req = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(req.as_bytes()).unwrap();
        let mut raw = Vec::new();
        let _ = stream.read_to_end(&mut raw);
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, resp_body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|c| c.parse::<u16>().ok())
            .unwrap_or(0);
        (status, resp_body.to_owned())
    })
    .await
    .unwrap()
}

pub fn assistant_content(body: &str) -> String {
    let parsed: Value = serde_json::from_str(body)
        .unwrap_or_else(|e| panic!("OpenAI response is not JSON ({e}): {body}"));
    parsed["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}

pub fn tool_outputs(req: &RecordedRequest) -> String {
    req.messages
        .iter()
        .filter(|m| m.get("role").and_then(Value::as_str) == Some("tool"))
        .filter_map(|m| m.get("content").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn read_all_files_under(root: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.is_file()
                && let Ok(text) = fs::read_to_string(&p)
            {
                out.push((p, text));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}
