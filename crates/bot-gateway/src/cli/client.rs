//! Unix-socket client protocol for `bot chat`.
//!
//! Keep the write half open after sending the request line — the server treats EOF as disconnect and stops waiting.
//! Before writing any prompt text, verify directory/socket ownership (`socket::trusted_socket`) and peer UID so a
//! socket pre-created by another user in a shared directory cannot intercept messages.
use super::socket::{current_uid, trusted_socket};
use serde_json::{Value, json};
use std::{io, path::Path};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

pub struct Chat {
    pub profile: Option<String>,
    /// Stable idempotency key across retries (`new_request_id`).
    pub request: String,
    pub thread: Option<String>,
    pub text: String,
    /// When false, returns immediately after durable acceptance.
    pub wait: bool,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Accepted,
    Done {
        state: String,
        result: Option<Value>,
    },
    Failed {
        code: String,
        message: String,
        retry_after_secs: Option<u64>,
    },
}

pub async fn chat(socket: &Path, chat: &Chat, on_event: impl FnMut(&Value)) -> io::Result<Outcome> {
    let mut request = json!({"v": super::VERSION, "op": "chat", "request": chat.request, "text": chat.text, "wait": chat.wait});
    if let Some(profile) = &chat.profile {
        request["profile"] = json!(profile);
    }
    if let Some(thread) = &chat.thread {
        request["thread"] = json!(thread);
    }
    exchange(socket, request, chat.wait, on_event).await
}

pub async fn result(
    socket: &Path,
    profile: Option<&str>,
    request: &str,
    wait: bool,
    on_event: impl FnMut(&Value),
) -> io::Result<Outcome> {
    let mut message =
        json!({"v": super::VERSION, "op": "result", "request": request, "wait": wait});
    if let Some(profile) = profile {
        message["profile"] = json!(profile);
    }
    exchange(socket, message, wait, on_event).await
}

async fn exchange(
    socket: &Path,
    request: Value,
    wait: bool,
    on_event: impl FnMut(&Value),
) -> io::Result<Outcome> {
    exchange_with(current_uid()?, socket, request, wait, on_event, peer_uid).await
}

fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    stream.peer_cred().map(|cred| cred.uid())
}

async fn exchange_with(
    uid: u32,
    socket: &Path,
    request: Value,
    wait: bool,
    mut on_event: impl FnMut(&Value),
    peer: impl FnOnce(&UnixStream) -> io::Result<u32>,
) -> io::Result<Outcome> {
    trusted_socket(socket, uid)?;
    let stream = UnixStream::connect(socket).await?;
    if peer(&stream)? != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "socket peer is not a server running as the current user",
        ));
    }
    let (read, mut write) = stream.into_split();
    let mut line = request.to_string();
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    write.flush().await?;
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        let value: Value = serde_json::from_str(&line)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        on_event(&value);
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_owned();
        match value["type"].as_str() {
            Some("accepted") if !wait => return Ok(Outcome::Accepted),
            Some("result") => {
                let result = match &value["result"] {
                    Value::Null => None,
                    other => Some(other.clone()),
                };
                return Ok(Outcome::Done {
                    state: text("state"),
                    result,
                });
            }
            Some("error") => {
                return Ok(Outcome::Failed {
                    code: text("code"),
                    message: text("message"),
                    retry_after_secs: value["retry_after_secs"].as_u64(),
                });
            }
            _ => {}
        }
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "server closed the connection before completing its response",
    ))
}

pub fn render(result: &Value) -> String {
    match result {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Object(map) if map.get("text").is_some_and(Value::is_string) => {
            let mut out = map["text"].as_str().unwrap_or_default().to_owned();
            for file in map
                .get("attachments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let name = file["name"].as_str().unwrap_or("attachment");
                out.push_str(&format!("\n[attachment] {name}"));
            }
            out
        }
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

pub fn new_request_id() -> io::Result<String> {
    Ok(format!("cli-{}", bot_core::random::random_hex(16)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_text_attachments_and_raw_json() {
        assert_eq!(render(&json!("hello")), "hello");
        assert_eq!(render(&json!({"text": "your agenda"})), "your agenda");
        assert_eq!(
            render(&json!({"text": "report", "attachments": [{"name": "a.txt"}, {}]})),
            "report\n[attachment] a.txt\n[attachment] attachment"
        );
        assert_eq!(render(&Value::Null), "");
        assert!(render(&json!({"rows": [1, 2]})).contains("\"rows\""));
        assert!(render(&json!({"text": 5})).contains("\"text\""));
    }

    fn fake_server(
        path: &Path,
        reply: Option<&'static str>,
    ) -> std::thread::JoinHandle<(usize, Vec<u8>)> {
        use std::{io::Read, io::Write, time::Instant};
        let listener = std::os::unix::net::UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let mut seen = (0usize, Vec::new());
            let deadline = Instant::now() + std::time::Duration::from_millis(800);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        seen.0 += 1;
                        let _ = stream.set_nonblocking(false);
                        let _ =
                            stream.set_read_timeout(Some(std::time::Duration::from_millis(300)));
                        let mut byte = [0u8; 1];
                        while let Ok(1) = stream.read(&mut byte) {
                            seen.1.push(byte[0]);
                            if byte[0] == b'\n' {
                                break;
                            }
                        }
                        if let (Some(reply), false) = (reply, seen.1.is_empty()) {
                            let _ = writeln!(stream, "{reply}");
                        }
                        break;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            seen
        })
    }

    fn private_dir(mode: u32) -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("c")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        dir
    }

    fn secret_request() -> Value {
        json!({"v": 1, "op": "chat", "request": "r1", "text": "secret chat", "wait": false})
    }

    #[tokio::test]
    async fn a_server_that_is_not_my_account_never_receives_the_request() {
        let uid = current_uid().unwrap();
        let dir = private_dir(0o700);
        let path = dir.path().join("s.sock");
        let server = fake_server(&path, Some(r#"{"type":"accepted"}"#));
        let refused = exchange_with(uid, &path, secret_request(), false, |_| {}, |_| Ok(uid + 1))
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        let (connections, bytes) = server.join().unwrap();
        assert_eq!(connections, 1, "peer check happens after connecting");
        assert!(bytes.is_empty(), "{}", String::from_utf8_lossy(&bytes));

        let path = dir.path().join("t.sock");
        let server = fake_server(&path, Some(r#"{"type":"accepted"}"#));
        let outcome = exchange_with(uid, &path, secret_request(), false, |_| {}, peer_uid)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Accepted);
        let (_, bytes) = server.join().unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("secret chat"));
    }

    #[tokio::test]
    async fn an_untrusted_folder_or_socket_is_refused_before_connecting() {
        let uid = current_uid().unwrap();
        let shared = private_dir(0o777);
        let path = shared.path().join("s.sock");
        let server = fake_server(&path, Some(r#"{"type":"accepted"}"#));
        let refused = exchange_with(uid, &path, secret_request(), false, |_| {}, peer_uid)
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(server.join().unwrap(), (0, Vec::new()));

        let mine = private_dir(0o700);
        let path = mine.path().join("s.sock");
        let server = fake_server(&path, Some(r#"{"type":"accepted"}"#));
        let refused = exchange_with(uid + 1, &path, secret_request(), false, |_| {}, peer_uid)
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(server.join().unwrap(), (0, Vec::new()));

        let none = mine.path().join("none.sock");
        let missing = exchange_with(uid, &none, secret_request(), false, |_| {}, peer_uid)
            .await
            .unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn request_ids_are_valid_and_unique() {
        let a = new_request_id().unwrap();
        let b = new_request_id().unwrap();
        assert_ne!(a, b);
        assert!(crate::inbound::valid_request_id(&a));
        assert_eq!(a.len(), 4 + 32);
    }
}
