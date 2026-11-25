//! SSE 응답을 이벤트 단위로 읽는 시험용 클라이언트. HTTP 청크 경계와 이벤트 경계는 다를 수 있으므로(한 청크에 이벤트 여럿) 바이트를 이어 붙여 빈 줄로 나눈다.
use super::{Rig, head_of};
use serde_json::Value;
use std::{collections::VecDeque, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

/// 응답을 이벤트 단위로 읽는 스트림 클라이언트(SSE)
pub struct Sse {
    reader: BufReader<TcpStream>,
    /// 아직 이벤트 하나로 끝맺지 못한 받은 글
    partial: String,
    /// 읽었지만 아직 돌려주지 않은 이벤트(끝의 빈 줄 포함)
    ready: VecDeque<String>,
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug)]
pub enum Next {
    Data(String),
    End,
    Idle,
}

impl Sse {
    pub async fn open(rig: &Rig, key: &str, idem: Option<&str>, body: &Value) -> Sse {
        let body = body.to_string();
        let mut text = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: test\r\nAuthorization: Bearer {key}\r\nContent-Type: application/json\r\nConnection: close\r\n"
        );
        if let Some(idem) = idem {
            text.push_str(&format!("Idempotency-Key: {idem}\r\n"));
        }
        text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        let mut stream = TcpStream::connect(rig.addr).await.unwrap();
        stream.write_all(text.as_bytes()).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut head = String::new();
        loop {
            let mut line = String::new();
            let n = tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
                .await
                .expect("머리 시간 초과")
                .unwrap();
            assert!(n > 0, "머리 도중 연결이 끊겼어요: {head:?}");
            if line == "\r\n" {
                break;
            }
            head.push_str(&line);
        }
        let (status, headers, _) = head_of(&format!("{head}\r\n\r\n"));
        Sse {
            reader,
            partial: String::new(),
            ready: VecDeque::new(),
            status,
            headers,
        }
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// 다음 이벤트 하나를 `within` 안에 읽는다(이벤트는 끝의 빈 줄까지 포함한다).
    pub async fn next(&mut self, within: Duration) -> Next {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if let Some(event) = self.ready.pop_front() {
                return Next::Data(event);
            }
            let read = async {
                let mut size = String::new();
                if self.reader.read_line(&mut size).await.unwrap() == 0 {
                    return None;
                }
                let size = usize::from_str_radix(size.trim(), 16).expect("청크 크기");
                if size == 0 {
                    return None;
                }
                let mut data = vec![0u8; size + 2];
                self.reader.read_exact(&mut data).await.unwrap();
                data.truncate(size);
                Some(String::from_utf8(data).unwrap())
            };
            match tokio::time::timeout_at(deadline, read).await {
                Err(_) => return Next::Idle,
                Ok(None) => {
                    // 끝났다: 끝맺지 못한 글이 남았으면 그대로 돌려준다(시험이 잘린 스트림을 알아본다)
                    if self.partial.is_empty() {
                        return Next::End;
                    }
                    return Next::Data(std::mem::take(&mut self.partial));
                }
                Ok(Some(chunk)) => {
                    self.partial.push_str(&chunk);
                    while let Some(at) = self.partial.find("\n\n") {
                        let event: String = self.partial.drain(..at + 2).collect();
                        self.ready.push_back(event);
                    }
                }
            }
        }
    }

    /// 스트림이 끝날 때까지 모든 이벤트
    pub async fn rest(&mut self, within: Duration) -> Vec<String> {
        let mut all = Vec::new();
        loop {
            match self.next(within).await {
                Next::Data(chunk) => all.push(chunk),
                Next::End => return all,
                Next::Idle => panic!("스트림이 {within:?} 안에 끝나지 않았어요: {all:?}"),
            }
        }
    }
}

/// `data: {json}\n\n` 한 줄을 JSON으로(아니면 None)
pub fn data_json(chunk: &str) -> Option<Value> {
    let line = chunk.strip_prefix("data: ")?;
    assert!(
        line.ends_with("\n\n") && line.matches('\n').count() == 2,
        "{chunk:?}"
    );
    serde_json::from_str(line.trim_end()).ok()
}
