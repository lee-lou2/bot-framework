//! 아주 작은 HTTP/1.1 읽기·쓰기. 요청 한 건당 연결 하나이고 상한을 모두 건다.

use std::time::Duration;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

const MAX_HEAD: usize = 16 * 1024;

pub struct Request {
    pub method: String,
    /// 쿼리를 뺀 경로.
    pub path: String,
    /// 소문자 이름.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// 읽기 실패에 돌려줄 상태.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Status {
    Bad,
    Timeout,
    TooLarge,
    HeadTooLarge,
}

impl Status {
    pub fn code(self) -> u16 {
        match self {
            Status::Bad => 400,
            Status::Timeout => 408,
            Status::TooLarge => 413,
            Status::HeadTooLarge => 431,
        }
    }
    pub fn message(self) -> &'static str {
        match self {
            Status::Bad => "bad_request",
            Status::Timeout => "timeout",
            Status::TooLarge | Status::HeadTooLarge => "too_large",
        }
    }
}

fn token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

pub async fn read_request(
    stream: &mut TcpStream,
    max_body: usize,
    head_wait: Duration,
    body_wait: Duration,
) -> Result<Request, Status> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let end = timeout(head_wait, async {
        let mut chunk = [0u8; 2048];
        loop {
            if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok(at);
            }
            if buf.len() > MAX_HEAD {
                return Err(Status::HeadTooLarge);
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return Err(Status::Bad),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
    })
    .await
    .map_err(|_| Status::Timeout)??;
    if end + 4 > MAX_HEAD {
        return Err(Status::HeadTooLarge);
    }
    let leftover = buf[end + 4..].to_vec();
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| Status::Bad)?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().ok_or(Status::Bad)?.split(' ');
    let (method, target, version) = (
        first.next().ok_or(Status::Bad)?,
        first.next().ok_or(Status::Bad)?,
        first.next().ok_or(Status::Bad)?,
    );
    if first.next().is_some() || !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return Err(Status::Bad);
    }
    let path = target.split(['?', '#']).next().unwrap_or("/").to_owned();
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(Status::Bad)?;
        if name.is_empty() || !name.bytes().all(token_char) {
            return Err(Status::Bad);
        }
        headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
    }
    let mut request = Request {
        method: method.to_owned(),
        path,
        headers,
        body: Vec::new(),
    };
    let lengths: Vec<&str> = request
        .headers
        .iter()
        .filter(|(k, _)| k == "content-length")
        .map(|(_, v)| v.as_str())
        .collect();
    let transfer_encodings: Vec<&str> = request
        .headers
        .iter()
        .filter(|(k, _)| k == "transfer-encoding")
        .map(|(_, v)| v.as_str())
        .collect();
    let chunked =
        transfer_encodings.len() == 1 && transfer_encodings[0].eq_ignore_ascii_case("chunked");
    if lengths.len() > 1
        || transfer_encodings.len() > 1
        || (chunked && !lengths.is_empty())
        || (!transfer_encodings.is_empty() && !chunked)
    {
        return Err(Status::Bad);
    }
    if request
        .header("expect")
        .is_some_and(|v| v.eq_ignore_ascii_case("100-continue"))
    {
        let _ = timeout(
            head_wait,
            stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n"),
        )
        .await;
    }
    request.body = timeout(body_wait, async {
        if chunked {
            read_chunked(stream, leftover, max_body).await
        } else {
            let length = match lengths.first() {
                None => 0,
                Some(v) if v.bytes().all(|b| b.is_ascii_digit()) && !v.is_empty() => {
                    v.parse::<usize>().map_err(|_| Status::Bad)?
                }
                Some(_) => return Err(Status::Bad),
            };
            if length > max_body {
                return Err(Status::TooLarge);
            }
            let mut body = leftover;
            if body.len() > length {
                return Err(Status::Bad);
            }
            let mut chunk = [0u8; 8192];
            while body.len() < length {
                let want = (length - body.len()).min(chunk.len());
                match stream.read(&mut chunk[..want]).await {
                    Ok(0) | Err(_) => return Err(Status::Bad),
                    Ok(n) => body.extend_from_slice(&chunk[..n]),
                }
            }
            Ok(body)
        }
    })
    .await
    .map_err(|_| Status::Timeout)??;
    Ok(request)
}

/// 청크 본문. 확장·트레일러는 받지 않는다.
async fn read_chunked(
    stream: &mut TcpStream,
    mut buf: Vec<u8>,
    max_body: usize,
) -> Result<Vec<u8>, Status> {
    let mut body = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        // 크기 줄
        let line_end = loop {
            if let Some(at) = buf.windows(2).position(|w| w == b"\r\n") {
                break at;
            }
            if buf.len() > 32 {
                return Err(Status::Bad);
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return Err(Status::Bad),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let size_text = std::str::from_utf8(&buf[..line_end]).map_err(|_| Status::Bad)?;
        if size_text.is_empty() || !size_text.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Status::Bad);
        }
        let size = usize::from_str_radix(size_text, 16).map_err(|_| Status::Bad)?;
        buf.drain(..line_end + 2);
        if size == 0 {
            // 마지막 빈 줄
            while buf.len() < 2 {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return Err(Status::Bad),
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            return if buf.starts_with(b"\r\n") {
                Ok(body)
            } else {
                Err(Status::Bad)
            };
        }
        if body.len().checked_add(size).is_none_or(|n| n > max_body) {
            return Err(Status::TooLarge);
        }
        let need = size.checked_add(2).ok_or(Status::TooLarge)?;
        while buf.len() < need {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return Err(Status::Bad),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        if &buf[size..need] != b"\r\n" {
            return Err(Status::Bad);
        }
        body.extend_from_slice(&buf[..size]);
        buf.drain(..need);
    }
}

pub async fn write_all(
    stream: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
    wait: Duration,
) -> bool {
    matches!(timeout(wait, stream.write_all(bytes)).await, Ok(Ok(())))
}

pub async fn write_chunk(
    stream: &mut (impl AsyncWrite + Unpin),
    data: &[u8],
    wait: Duration,
) -> bool {
    let mut framed = format!("{:x}\r\n", data.len()).into_bytes();
    framed.extend_from_slice(data);
    framed.extend_from_slice(b"\r\n");
    write_all(stream, &framed, wait).await
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

pub async fn write_response(
    stream: &mut TcpStream,
    code: u16,
    content_type: &str,
    body: &[u8],
    wait: Duration,
) {
    let head = format!(
        "HTTP/1.1 {code} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reason(code),
        body.len()
    );
    let mut all = head.into_bytes();
    all.extend_from_slice(body);
    let _ = write_all(stream, &all, wait).await;
    let _ = stream.shutdown().await;
}

/// 상대가 연결을 닫을 때까지 기다린다(읽은 바이트는 버린다). 스트림 도중 클라이언트가 떠났는지 알아챈다.
pub async fn closed(stream: &mut (impl AsyncRead + Unpin)) {
    let mut sink = [0u8; 256];
    loop {
        match stream.read(&mut sink).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn feed(raw: &[u8], max_body: usize) -> Result<Request, Status> {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let data = raw.to_vec();
        let writer = tokio::spawn(async move {
            let mut client = TcpStream::connect(addr).await.unwrap();
            let _ = client.write_all(&data).await;
            let _ = client.shutdown().await;
        });
        let (mut server, _) = listener.accept().await.unwrap();
        let res = read_request(
            &mut server,
            max_body,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
        .await;
        let _ = writer.await;
        res
    }

    #[tokio::test]
    async fn duplicate_transfer_encoding_headers_are_rejected() {
        for raw in [
            b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".as_slice(),
            b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: gzip\r\n\r\n0\r\n\r\n".as_slice(),
            b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: gzip\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n".as_slice(),
        ] {
            assert_eq!(feed(raw, 1024).await.err(), Some(Status::Bad));
        }
    }

    #[tokio::test]
    async fn overflowing_chunk_size_returns_too_large_without_wrapping() {
        for raw in [
            b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\n"
                .as_slice(),
            b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n1\r\na\r\nffffffffffffffff\r\n"
                .as_slice(),
            b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nfffffffffffffffe\r\n"
                .as_slice(),
        ] {
            assert_eq!(feed(raw, 1024).await.err(), Some(Status::TooLarge));
        }
        // 선행 `+`가 붙은 청크 크기는 16진수 규격이 아니다
        let plus = b"POST /mcp HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n+1\r\na\r\n0\r\n\r\n";
        assert_eq!(feed(plus, 1024).await.err(), Some(Status::Bad));
    }
}
