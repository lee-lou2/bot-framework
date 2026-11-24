//! 가장 작은 HTTP/1.1 POST 클라이언트(평문 `http://` 전용). OTLP 수집기는 루프백·사설망에 두고 TLS가 필요하면
//! 수집기(sidecar)가 맡는다. 오류에는 주소·헤더를 싣지 않고 고정 분류 낱말만 돌려준다.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Target {
    pub host: String,
    pub port: u16,
    pub path: String,
}

fn host_ok(h: &str) -> bool {
    !h.is_empty()
        && h.len() <= 253
        && h.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b':'))
}

fn local_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => local_ip(IpAddr::V4(v4)),
            None => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
        },
    }
}

impl Target {
    /// `http://호스트[:포트][/경로]`만 받는다. 사용자 정보·쿼리·조각·`https://`는 거절한다.
    pub(super) fn parse(url: &str) -> Result<Self, &'static str> {
        let rest = url
            .strip_prefix("http://")
            .ok_or("평문 http://만 지원한다(TLS는 수집기에서 처리)")?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/v1/traces"),
        };
        if path.contains(['?', '#', ' ', '\r', '\n']) || authority.contains('@') {
            return Err("주소에 사용자 정보·쿼리·조각을 넣을 수 없다");
        }
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, tail) = v6.split_once(']').ok_or("잘못된 IPv6 주소")?;
            (h, tail.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        let port = match port {
            Some(p) => p
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or("잘못된 포트")?,
            None => 80,
        };
        if !host_ok(host) {
            return Err("잘못된 호스트");
        }
        Ok(Self {
            host: host.to_owned(),
            port,
            path: path.to_owned(),
        })
    }

    /// 평문으로 보내도 사설망 안에 머무는 주소인가: 루프백·사설(RFC 1918)·링크 로컬·IPv6 고유 로컬 IP 리터럴, 그리고 `localhost`·`*.localhost`(표준상 루프백).
    /// 그 밖의 이름(DNS)은 어디로 풀릴지 알 수 없어 «아니다»(풀린 주소를 믿지 않으려고 글자만 본다). `127.0.0.1.example.com`·`2130706433`·`010.0.0.1`처럼
    /// 루프백·사설처럼 보이지만 IP 리터럴이 아닌 것도 «아니다».
    pub(super) fn stays_local(&self) -> bool {
        match self.host.parse::<IpAddr>() {
            Ok(ip) => local_ip(ip),
            Err(_) => self.is_localhost_name(),
        }
    }

    /// `localhost`·`*.localhost`(대소문자 무시): 표준(RFC 6761)이 루프백으로 못 박은 이름.
    fn is_localhost_name(&self) -> bool {
        let name = self.host.to_ascii_lowercase();
        name == "localhost" || name.ends_with(".localhost")
    }

    /// 이름 해석 없이 접속할 주소(있으면). IP 리터럴은 적힌 그대로, `localhost`·`*.localhost`는 시스템 해석기(hosts 파일·DNS)가 무엇을 돌려주든 **루프백
    /// `127.0.0.1`·`::1`로 고정**한다 — 허용 검사(`stays_local`)는 이름만 보므로, 해석기가 비루프백을 돌려주면 평문 헤더·본문이 그쪽으로 갈 수 있다.
    /// `Host` 헤더에는 원래 이름이 그대로 실린다. 그 밖의 이름은 `None`(해석기에 맡긴다 — 운영자가 `remote`로 명시한 경우뿐이다).
    pub(super) fn dial(&self) -> Option<Vec<SocketAddr>> {
        match self.host.parse::<IpAddr>() {
            Ok(ip) => Some(vec![SocketAddr::new(ip, self.port)]),
            Err(_) if self.is_localhost_name() => Some(vec![
                SocketAddr::new(Ipv4Addr::LOCALHOST.into(), self.port),
                SocketAddr::new(Ipv6Addr::LOCALHOST.into(), self.port),
            ]),
            Err(_) => None,
        }
    }

    /// 표시용 출처(`호스트:포트`).
    pub(super) fn origin(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// 헤더 이름·값 검사: 이름은 `[A-Za-z0-9-]`, 값에는 줄바꿈·NUL이 없다.
pub(super) fn header_ok(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
        && value.len() <= 4096
        && !value.bytes().any(|c| matches!(c, b'\r' | b'\n' | 0))
}

/// 한 번 보낸다. 성공은 2xx뿐이고 실패는 `connect`·`timeout`·`io`·`status` 중 하나다.
pub(super) async fn post(
    t: &Target,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
) -> Result<(), &'static str> {
    tokio::time::timeout(timeout, async {
        let mut s = match t.dial() {
            Some(addrs) => TcpStream::connect(&addrs[..]).await,
            None => TcpStream::connect((t.host.as_str(), t.port)).await,
        }
        .map_err(|_| "connect")?;
        let mut head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/x-protobuf\r\nContent-Length: {}\r\nConnection: close\r\n",
            t.path,
            t.origin(),
            body.len()
        );
        for (k, v) in headers {
            if header_ok(k, v) {
                head.push_str(&format!("{k}: {v}\r\n"));
            }
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).await.map_err(|_| "io")?;
        s.write_all(body).await.map_err(|_| "io")?;
        let (mut buf, mut chunk) = (Vec::new(), [0u8; 512]);
        while !buf.windows(2).any(|w| w == b"\r\n") && buf.len() < 4096 {
            match s.read(&mut chunk).await {
                Ok(0) | Err(_) => return Err("io"),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let line = String::from_utf8_lossy(&buf);
        let code = line.split(' ').nth(1).and_then(|c| c.trim().parse::<u16>().ok());
        match code {
            Some(200..=299) => Ok(()),
            Some(_) => Err("status"),
            None => Err("io"),
        }
    })
    .await
    .unwrap_or(Err("timeout"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_http_endpoints_are_accepted() {
        let t = Target::parse("http://127.0.0.1:4318/v1/traces").expect("정상");
        assert_eq!(
            (t.host.as_str(), t.port, t.path.as_str()),
            ("127.0.0.1", 4318, "/v1/traces")
        );
        assert_eq!(
            Target::parse("http://collector.internal")
                .expect("기본")
                .path,
            "/v1/traces"
        );
        assert_eq!(
            Target::parse("http://collector.internal")
                .expect("기본")
                .port,
            80
        );
        let v6 = Target::parse("http://[::1]:4318/x").expect("IPv6");
        assert_eq!(
            (v6.host.as_str(), v6.origin().as_str()),
            ("::1", "[::1]:4318")
        );
        for bad in [
            "https://collector:4318",
            "ftp://x",
            "collector:4318",
            "http://",
            "http://user:pw@collector:4318",
            "http://collector:0",
            "http://collector:99999",
            "http://collector:4318/v1/traces?x=1",
            "http://col lector:4318",
            "http://[::1",
        ] {
            assert!(Target::parse(bad).is_err(), "{bad}");
        }
    }

    fn target(url: &str) -> Target {
        Target::parse(url).expect("정상")
    }

    fn addrs(list: &[&str]) -> Option<Vec<SocketAddr>> {
        Some(list.iter().map(|a| a.parse().expect("주소")).collect())
    }

    /// 허용 검사는 이름만 본다. 그래서 `localhost`·`*.localhost`는 해석기가 무엇을 돌려주든 루프백으로 고정해 접속하고, `Host` 헤더만 원래 이름을 둔다.
    #[test]
    fn localhost_names_are_dialed_at_loopback_without_asking_the_resolver() {
        for url in [
            "http://localhost:4318/v1/traces",
            "http://LOCALHOST:4318",
            "http://collector.localhost:4318",
            "http://a.b.c.LocalHost:4318",
        ] {
            let t = target(url);
            assert!(t.stays_local(), "{url}");
            assert_eq!(
                t.dial(),
                addrs(&["127.0.0.1:4318", "[::1]:4318"]),
                "{url}: 루프백 둘(IPv4 먼저)로 고정"
            );
        }
        // 접속 주소와 상관없이 Host 헤더에는 원래 이름과 포트가 실린다
        let t = target("http://Collector.localhost:4318/x");
        assert_eq!(t.origin(), "Collector.localhost:4318");
        assert_eq!(
            target("http://localhost:1").dial(),
            addrs(&["127.0.0.1:1", "[::1]:1"]),
            "포트는 적은 그대로"
        );
    }

    #[test]
    fn ip_literals_are_dialed_exactly_as_written_and_other_names_are_left_to_the_resolver() {
        for (url, expected) in [
            ("http://127.0.0.1:4318", "127.0.0.1:4318"),
            ("http://10.1.2.3:80", "10.1.2.3:80"),
            ("http://203.0.113.7:9", "203.0.113.7:9"),
            ("http://[::1]:4318", "[::1]:4318"),
            ("http://[fd00::1]:4318", "[fd00::1]:4318"),
        ] {
            assert_eq!(target(url).dial(), addrs(&[expected]), "{url}");
        }
        // 이름은 해석기에 맡긴다(허용되려면 운영자가 `remote`를 명시해야 한다). 루프백처럼 보이는 이름도 고정하지 않는다
        for url in [
            "http://collector.example.com:4318",
            "http://otel-collector:4318",
            "http://127.0.0.1.example.com:4318",
            "http://notlocalhost:4318",
            "http://localhost.example.com:4318",
            "http://2130706433:4318",
        ] {
            let t = target(url);
            assert_eq!(t.dial(), None, "{url}");
            assert!(!t.stays_local(), "{url}");
        }
    }

    #[test]
    fn header_names_and_values_cannot_smuggle_lines() {
        assert!(header_ok("Authorization", "Bearer abc"));
        assert!(header_ok("x-api-key", "k"));
        for (n, v) in [
            ("", "v"),
            ("a b", "v"),
            ("a:b", "v"),
            ("a", "v\r\nX: y"),
            ("a", "v\n"),
            ("a", "v\0"),
        ] {
            assert!(!header_ok(n, v), "{n:?}");
        }
    }
}
