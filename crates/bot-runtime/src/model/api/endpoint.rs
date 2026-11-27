//! 상류 기준 주소(`providers.<별칭>.endpoint`)의 검증. 이 값이 대화와 서비스 키가 나가는 유일한 목적지라 모양을 좁게 받는다.
//!
//! `https://호스트[:포트][/경로]`만 쓴다. `http://`는 호스트가 **loopback IP 리터럴**(127.0.0.0/8·`::1`)일 때만 받는다(로컬 Ollama·시험): `localhost` 같은 이름이나
//! 사설망 주소는 이름 해석·망 구성이 바뀌면 다른 곳을 가리킬 수 있어 평문으로 보내지 않는다. 자격(`user:pass@`)·쿼리·조각·퍼센트 인코딩·공백·제어 문자·비ASCII는 거절한다.
//! 호스트 이름은 소문자 DNS 이름이어야 하고, `127.1`·`2130706433`·`0x7f000001`처럼 IP 주소의 변형 표기로 읽힐 수 있는 숫자·16진수(`0x`) 마지막 레이블은 거절한다.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 주소 전체 길이 상한
const MAX_LEN: usize = 2048;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scheme {
    Http,
    Https,
}

/// 검증을 마친 상류 기준 주소. `parse`가 유일한 생성 경로라 이 값이 있으면 위 규칙을 지킨 것이다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApiEndpoint {
    scheme: Scheme,
    /// 소문자 DNS 이름 또는 IP 리터럴(대괄호 없이)
    host: String,
    port: u16,
    /// 비었거나 `/a/b` 꼴(끝 슬래시 없음)
    path: String,
    loopback: bool,
}

impl ApiEndpoint {
    /// 설정의 주소 글을 해석한다. 거절 사유는 고정 문장이고 입력을 되풀이하지 않는다(자격이 섞여 있어도 새지 않게).
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty()
            || text.len() > MAX_LEN
            || !text.is_ascii()
            || text.bytes().any(|b| b <= b' ' || b == 0x7f || b == b'\\')
        {
            return Err("공백·제어 문자·백슬래시가 없는 ASCII 주소여야 해요".to_owned());
        }
        if text.contains(['?', '#', '@', '%']) {
            return Err("자격(user@)·쿼리(?)·조각(#)·퍼센트 인코딩(%)은 쓸 수 없어요".to_owned());
        }
        let (scheme, rest) = if let Some(rest) = text.strip_prefix("https://") {
            (Scheme::Https, rest)
        } else if let Some(rest) = text.strip_prefix("http://") {
            (Scheme::Http, rest)
        } else {
            return Err(
                "`https://호스트[:포트][/경로]`여야 해요(`http://`는 loopback IP 주소만)"
                    .to_owned(),
            );
        };
        let (authority, path) = match rest.find('/') {
            Some(at) => (&rest[..at], &rest[at..]),
            None => (rest, ""),
        };
        let (host, port) = split_authority(authority)?;
        let port = match port {
            Some(port) => port,
            None if scheme == Scheme::Https => 443,
            None => 80,
        };
        let loopback = matches!(&host, Host::Ip(ip) if ip.is_loopback());
        if scheme == Scheme::Http && !loopback {
            return Err(
                "`http://`는 loopback IP 주소(127.0.0.0/8·[::1])에만 쓸 수 있어요 — 그 밖의 호스트는 `https://`여야 해요"
                    .to_owned(),
            );
        }
        Ok(Self {
            scheme,
            host: match host {
                Host::Ip(ip) => ip.to_string(),
                Host::Name(name) => name,
            },
            port,
            path: base_path(path)?,
            loopback,
        })
    }

    /// 키 없이 쓸 수 있는 주소인가(호스트가 loopback IP 리터럴이다).
    pub fn is_loopback(&self) -> bool {
        self.loopback
    }

    /// 기준 주소에 요청 경로(`/v1/messages`)를 이어 붙인 전체 주소. IPv6 리터럴은 대괄호로 감싼다.
    pub fn url(&self, suffix: &str) -> String {
        let scheme = match self.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
        };
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!("{scheme}://{host}:{}{}{suffix}", self.port, self.path)
    }
}

enum Host {
    Ip(IpAddr),
    Name(String),
}

/// `호스트[:포트]`·`[IPv6][:포트]`를 나눈다. 포트는 1~65535의 숫자여야 한다.
fn split_authority(authority: &str) -> Result<(Host, Option<u16>), String> {
    let (host_text, port_text, bracketed) = if let Some(inner) = authority.strip_prefix('[') {
        let Some((host, after)) = inner.split_once(']') else {
            return Err("IPv6 주소의 대괄호가 닫히지 않았어요".to_owned());
        };
        let port = match after {
            "" => None,
            _ => Some(
                after
                    .strip_prefix(':')
                    .ok_or_else(|| "대괄호 뒤에는 `:포트`만 올 수 있어요".to_owned())?,
            ),
        };
        (host, port, true)
    } else {
        if authority.matches(':').count() > 1 {
            return Err("IPv6 주소는 대괄호로 감싸야 해요".to_owned());
        }
        match authority.split_once(':') {
            Some((host, port)) => (host, Some(port), false),
            None => (authority, None, false),
        }
    };
    let port = match port_text {
        None => None,
        Some(text) => {
            if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
                return Err("포트는 1~65535의 숫자여야 해요".to_owned());
            }
            match text.parse::<u16>() {
                Ok(port) if port != 0 => Some(port),
                _ => return Err("포트는 1~65535의 숫자여야 해요".to_owned()),
            }
        }
    };
    let host = if bracketed {
        Host::Ip(IpAddr::V6(
            host_text
                .parse::<Ipv6Addr>()
                .map_err(|_| "대괄호 안은 IPv6 주소여야 해요".to_owned())?,
        ))
    } else if let Ok(ip) = host_text.parse::<Ipv4Addr>() {
        Host::Ip(IpAddr::V4(ip))
    } else {
        Host::Name(dns_name(host_text).ok_or_else(|| {
            "호스트는 DNS 이름(영문·숫자·하이픈, 마지막 레이블은 숫자만이면 안 돼요)이나 IP 주소여야 해요"
                .to_owned()
        })?)
    };
    Ok((host, port))
}

/// 소문자로 바꾼 DNS 이름. 레이블은 1~63자의 영문·숫자·하이픈(처음·끝은 하이픈이 아니다), 전체 253자 이하. 끝점(`.`)은 받지 않는다.
fn dns_name(name: &str) -> Option<String> {
    let name = name.to_ascii_lowercase();
    if name.is_empty() || name.len() > 253 {
        return None;
    }
    let labels: Vec<&str> = name.split('.').collect();
    for label in &labels {
        let ok = !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-');
        if !ok {
            return None;
        }
    }
    // 숫자뿐이거나 `0x` 16진수인 마지막 레이블은 `127.1`·`2130706433`·`0x7f000001`·`127.0x1` 같은 IP의 변형 표기로 해석될 수 있다
    let last = *labels.last()?;
    if last.bytes().all(|b| b.is_ascii_digit())
        || last
            .strip_prefix("0x")
            .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    Some(name)
}

/// 기준 경로: 비었거나 `/조각/조각`(끝 슬래시 하나는 떼고 받는다). 조각은 영문·숫자·`-_.~`만이고 빈 조각·`.`·`..`은 거절한다.
fn base_path(path: &str) -> Result<String, String> {
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    for segment in trimmed[1..].split('/') {
        let ok = !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'));
        if !ok {
            return Err(
                "경로 조각은 영문·숫자·`-_.~`만이고 비어 있거나 `.`·`..`일 수 없어요".to_owned(),
            );
        }
    }
    Ok(trimmed.to_owned())
}
