//! 제품 입구에 SDK 없이 직접 보내는 최소 HTTP/1.1 클라이언트(키를 바꾸고 지우는 시험용): 응답을 끝까지 읽는 것과, 보내기만 하고 끊는 것.
use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

/// (상태, 응답 머리 줄들, 본문)
pub struct Reply {
    pub status: u16,
    pub headers: Vec<String>,
    pub body: String,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }
}

fn message(
    method: &str,
    path: &str,
    key: &str,
    body: Option<&str>,
    extra: &[(&str, &str)],
) -> String {
    let body = body.unwrap_or("");
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    format!("{head}\r\n{body}")
}

/// 요청을 보내고 응답을 끝까지 읽는다(블로킹: `spawn_blocking`에서 부른다).
pub fn call(
    origin: &str,
    method: &str,
    path: &str,
    key: &str,
    body: Option<&str>,
    extra: &[(&str, &str)],
) -> Reply {
    let mut stream = TcpStream::connect(origin).expect("입구에 접속");
    stream
        .set_read_timeout(Some(Duration::from_secs(90)))
        .unwrap();
    stream
        .write_all(message(method, path, key, body, extra).as_bytes())
        .unwrap();
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Reply {
        status,
        headers: lines.map(str::to_owned).collect(),
        body: body.to_owned(),
    }
}

/// 요청을 보내고 응답을 기다리지 않고 끊는다(접수만 시키고 클라이언트가 사라지는 경우).
pub fn send_and_hang_up(origin: &str, path: &str, key: &str, body: &str, extra: &[(&str, &str)]) {
    let mut stream = TcpStream::connect(origin).expect("입구에 접속");
    stream
        .write_all(message("POST", path, key, Some(body), extra).as_bytes())
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
}
