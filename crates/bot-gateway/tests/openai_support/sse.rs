//! Event-oriented SSE test client that reassembles HTTP chunks and splits on blank lines.
use super::{Rig, head_of};
use serde_json::Value;
use std::{collections::VecDeque, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

pub struct Sse {
    reader: BufReader<TcpStream>,
    partial: String,
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
                .expect("header timeout")
                .unwrap();
            assert!(n > 0, "connection closed during headers: {head:?}");
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
                let size = usize::from_str_radix(size.trim(), 16).expect("chunk size");
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
                    // Return trailing unterminated bytes so tests can detect truncated streams.
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

    pub async fn rest(&mut self, within: Duration) -> Vec<String> {
        let mut all = Vec::new();
        loop {
            match self.next(within).await {
                Next::Data(chunk) => all.push(chunk),
                Next::End => return all,
                Next::Idle => panic!("stream did not finish within {within:?}: {all:?}"),
            }
        }
    }
}

pub fn data_json(chunk: &str) -> Option<Value> {
    let line = chunk.strip_prefix("data: ")?;
    assert!(
        line.ends_with("\n\n") && line.matches('\n').count() == 2,
        "{chunk:?}"
    );
    serde_json::from_str(line.trim_end()).ok()
}
