//! `bot chat`이 쓰는 클라이언트 쪽 프로토콜(서버와 한 크레이트에 둬서 모양이 어긋나지 않게 한다).
//! 요청 줄을 보낸 뒤 쓰기 쪽을 닫지 않는다 — 서버는 연결이 닫히면 끊긴 것으로 보고 기다림을 멈춘다.
//!
//! **서버를 믿을 수 있어야 글을 보낸다**: 대화 원문은 소켓 반대편에 그대로 가므로, 접속하기 전에 소켓 폴더·파일이 본인 소유의 비공개(0700) 것인지 점검하고
//! (`socket::trusted_socket`), 접속한 뒤에는 반대편 프로세스의 uid(피어 자격)가 자기 uid인지 확인한 다음에야 요청을 쓴다. 어긋나면 아무것도 보내지 않고
//! `PermissionDenied`로 끝난다 — 공용 폴더에 다른 사용자가 먼저 만들어 둔 가짜 서버가 대화를 가로채거나 가짜 답을 보여 주지 못한다.
use super::socket::{current_uid, trusted_socket};
use serde_json::{Value, json};
use std::{io, path::Path};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

pub struct Chat {
    pub profile: Option<String>,
    /// 재시도에도 같아야 한다. 새로 만들려면 `new_request_id`.
    pub request: String,
    pub thread: Option<String>,
    pub text: String,
    /// false면 접수까지만 확인하고 돌아온다.
    pub wait: bool,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// `wait: false` — 접수가 끝났다.
    Accepted,
    /// run이 끝났다. state는 succeeded·dead·cancelled.
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

/// 요청 하나를 보내고 결과까지(또는 접수까지) 받는다. 중간 이벤트는 `on_event`로 흘려 준다.
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

/// 같은 요청 ID의 결과를 다시 받는다(재부착). 새 접수는 만들지 않는다.
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

/// 접속한 소켓 반대편 프로세스의 uid(피어 자격)
fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    stream.peer_cred().map(|cred| cred.uid())
}

/// `exchange`의 몸통. `uid`는 서버가 돌아야 하는 계정(= 이 프로세스의 uid)이고, `peer`는 접속한 반대편의 uid를 묻는 함수다(시험이 다른 계정을 흉내 낼 수 있게 받는다).
async fn exchange_with(
    uid: u32,
    socket: &Path,
    request: Value,
    wait: bool,
    mut on_event: impl FnMut(&Value),
    peer: impl FnOnce(&UnixStream) -> io::Result<u32>,
) -> io::Result<Outcome> {
    // 접속하기 전: 폴더·소켓 파일. 믿을 수 없는 곳에는 접속조차 하지 않는다
    trusted_socket(socket, uid)?;
    let stream = UnixStream::connect(socket).await?;
    // 접속한 뒤, 아무것도 쓰기 전: 반대편이 내 계정의 프로세스여야 한다
    if peer(&stream)? != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "소켓 반대편이 내 계정으로 실행 중인 서버가 아니에요",
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
        "서버가 응답을 마치기 전에 연결을 닫았어요",
    ))
}

/// 결과 JSON을 사람이 읽을 글로. 문자열이면 그대로, `{"text": …}`면 그 글(+첨부 이름), 그 밖은 보기 좋은 JSON.
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
                let name = file["name"].as_str().unwrap_or("첨부");
                out.push_str(&format!("\n[첨부] {name}"));
            }
            out
        }
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

/// 재시도에도 같은 값을 쓰도록 호출자가 한 번 만들어 들고 있는 요청 ID(무작위 128비트).
pub fn new_request_id() -> io::Result<String> {
    Ok(format!("cli-{}", bot_core::random::random_hex(16)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_text_attachments_and_raw_json() {
        assert_eq!(render(&json!("안녕")), "안녕");
        assert_eq!(render(&json!({"text": "일정이에요"})), "일정이에요");
        assert_eq!(
            render(&json!({"text": "보고서", "attachments": [{"name": "a.txt"}, {}]})),
            "보고서\n[첨부] a.txt\n[첨부] 첨부"
        );
        assert_eq!(render(&Value::Null), "");
        assert!(render(&json!({"rows": [1, 2]})).contains("\"rows\""));
        assert!(render(&json!({"text": 5})).contains("\"text\""));
    }

    /// 소켓에서 한 연결만 받아 요청 한 줄을 읽고 `reply`가 있으면 돌려주는 가짜 서버. 받은 (연결 수, 바이트)를 돌려준다. 연결이 없으면 잠깐 기다리다 끝난다.
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
                        // 상대가 이미 끊었으면 이 설정이 실패할 수 있다(읽기는 곧 끝난다)
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
        json!({"v": 1, "op": "chat", "request": "r1", "text": "비밀 대화", "wait": false})
    }

    #[tokio::test]
    async fn a_server_that_is_not_my_account_never_receives_the_request() {
        let uid = current_uid().unwrap();
        let dir = private_dir(0o700);
        let path = dir.path().join("s.sock");
        let server = fake_server(&path, Some(r#"{"type":"accepted"}"#));
        // 폴더는 내 것이지만 반대편 프로세스의 uid가 내 uid가 아니다(다른 계정의 서버를 흉내 낸다)
        let refused = exchange_with(uid, &path, secret_request(), false, |_| {}, |_| Ok(uid + 1))
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        let (connections, bytes) = server.join().unwrap();
        assert_eq!(connections, 1, "점검은 접속한 뒤에 한다");
        assert!(bytes.is_empty(), "{}", String::from_utf8_lossy(&bytes));

        // 대조: 반대편이 내 계정이면 같은 길로 요청이 간다
        let path = dir.path().join("t.sock");
        let server = fake_server(&path, Some(r#"{"type":"accepted"}"#));
        let outcome = exchange_with(uid, &path, secret_request(), false, |_| {}, peer_uid)
            .await
            .unwrap();
        assert_eq!(outcome, Outcome::Accepted);
        let (_, bytes) = server.join().unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("비밀 대화"));
    }

    #[tokio::test]
    async fn an_untrusted_folder_or_socket_is_refused_before_connecting() {
        let uid = current_uid().unwrap();
        // 다른 사용자가 드나들 수 있는 폴더의 가짜 소켓: 접속조차 하지 않는다
        let shared = private_dir(0o777);
        let path = shared.path().join("s.sock");
        let server = fake_server(&path, Some(r#"{"type":"accepted"}"#));
        let refused = exchange_with(uid, &path, secret_request(), false, |_| {}, peer_uid)
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(server.join().unwrap(), (0, Vec::new()));

        // 내 비공개 폴더지만 그 폴더의 주인이 내가 아니라고 보는 쪽(`uid + 1`)이면 마찬가지다
        let mine = private_dir(0o700);
        let path = mine.path().join("s.sock");
        let server = fake_server(&path, Some(r#"{"type":"accepted"}"#));
        let refused = exchange_with(uid + 1, &path, secret_request(), false, |_| {}, peer_uid)
            .await
            .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(server.join().unwrap(), (0, Vec::new()));

        // 서버가 없으면 `NotFound`(점검 실패가 아니라 «서버를 띄우세요»)
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
