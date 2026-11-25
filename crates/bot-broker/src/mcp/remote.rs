//! 원격(Streamable HTTP) MCP 서버의 접속 대상. 호스트 설정 `servers.<별칭>`의 `{"transport": "http", "url": …, "auth": {"kind": "bearer", "secret": …}}`.
//!
//! 주소 규칙(위반하면 시작하지 않는다):
//! - `https`만. `http`는 **루프백 IPv4 주소**(`127.0.0.0/8`)일 때만(시험·로컬). `localhost` 같은 이름은 안 된다. IPv6 리터럴은 허용 목록(`호스트:포트`)에 적을 수 없어 쓰지 않는다.
//! - 사용자 정보(`user:pass@`)·조각(`#…`)은 거절한다. 인증은 `auth`로만 한다.
//! - 이름은 소문자 ASCII(IDN은 punycode)다. **숫자로 끝나는 호스트**(WHATWG URL의 «끝이 수인 호스트»: 마지막 라벨이 십진수이거나 `0x` 16진수)는 URL 파서(`reqwest`)와
//!   운영체제가 IPv4로 읽으므로 이름이 아니다: 점으로 나눈 십진수 네 마디의 정규 표기(`93.184.216.34`)만 받고, 16진·8진·정수·축약·끝의 점(`0x7f000001`·`2130706433`·
//!   `0177.0.0.1`·`127.1`·`1.2.3.4.`)은 거절한다. 정책이 «이름»으로 본 것을 연결 쪽이 IP로 읽어 이름 풀이 검사를 건너뛰는 틈이 없다.
//! - 접속하는 곳은 이 주소의 `호스트:포트` 하나뿐이다(리다이렉트를 따라가지 않는다).
use crate::{egress::canon_host, resolve::HostPort};
use bot_core::id::SecretName;
use sha2::{Digest, Sha256};
use std::net::Ipv4Addr;

const MAX_URL: usize = 2048;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Remote {
    /// `https`이면 true. `http`는 루프백 IPv4 주소일 때만 만들어진다.
    pub tls: bool,
    /// 정규화한 호스트: 이름은 소문자, IPv4는 정규 점-십진 표기
    pub host: String,
    pub port: u16,
    /// `/`로 시작하는 경로와 질의
    pub path: String,
    /// bearer 값을 담은 자격 이름(프로필 이름공간). 값은 요청 머리에만 들어간다. 없으면 인증 머리를 보내지 않는다.
    pub bearer: Option<SecretName>,
}

fn field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

impl Remote {
    /// 주소를 해석한다. 규칙을 어기면 사람이 읽을 이유를 돌려준다.
    pub fn parse(url: &str, bearer: Option<SecretName>) -> Result<Self, String> {
        if url.is_empty()
            || url.len() > MAX_URL
            || !url.is_ascii()
            || url.bytes().any(|b| b.is_ascii_control() || b == b' ')
        {
            return Err(format!(
                "url은 공백·제어 문자 없는 ASCII {MAX_URL}자 이하여야 해요(이름은 punycode로 적으세요)"
            ));
        }
        let (scheme, rest) = url
            .split_once("://")
            .ok_or("url은 `https://…` 모양이어야 해요")?;
        let tls = match scheme.to_ascii_lowercase().as_str() {
            "https" => true,
            "http" => false,
            _ => return Err("url은 https여야 해요(루프백 IPv4 주소만 http를 쓸 수 있어요)".into()),
        };
        if rest.contains('#') {
            return Err("url에 조각(`#…`)을 쓸 수 없어요".into());
        }
        let end = rest.find(['/', '?']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        if authority.contains('@') {
            return Err(
                "url에 사용자 정보(`user:pass@`)를 쓸 수 없어요 — 인증은 `auth.bearer`로 해요"
                    .into(),
            );
        }
        let (host, port) = split_authority(authority)?;
        if host.contains(':') {
            return Err("IPv6 리터럴 주소는 쓸 수 없어요(허용 목록 `호스트:포트`에 적을 수 없어요) — 이름이나 IPv4 주소를 쓰세요".into());
        }
        let port = match port {
            Some(text) => text
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0 && text.bytes().all(|b| b.is_ascii_digit()))
                .ok_or("url의 포트는 1~65535여야 해요")?,
            None if tls => 443,
            None => 80,
        };
        let lower = host.to_ascii_lowercase();
        let host = if ends_in_number(&lower) {
            // URL 파서와 운영체제가 IPv4로 읽는 호스트: 정규 점-십진 표기만(다른 표기는 같은 주소를 이름 풀이 검사 없이 가리킬 수 있다)
            let ip = lower
                .parse::<Ipv4Addr>()
                .ok()
                .filter(|ip| ip.to_string() == lower)
                .ok_or(
                    "url의 호스트가 비정규 숫자 표기예요 — 점으로 나눈 십진수 네 마디(`93.184.216.34`)만 쓸 수 있어요(16진·8진·정수·축약·끝의 점은 안 돼요)",
                )?;
            if !tls && !ip.is_loopback() {
                return Err("http는 루프백 IPv4 주소(`127.0.0.0/8`)만 쓸 수 있어요".into());
            }
            ip.to_string()
        } else if !tls {
            return Err(
                "http는 루프백 IPv4 주소(`127.0.0.0/8`)만 쓸 수 있어요 — 이름이면 https를 쓰세요"
                    .into(),
            );
        } else {
            named(host)?
        };
        let path = match tail {
            "" => "/".to_owned(),
            query if query.starts_with('?') => format!("/{query}"),
            path => path.to_owned(),
        };
        Ok(Self {
            tls,
            host,
            port,
            path,
            bearer,
        })
    }

    /// 허용 목록에 적는 `호스트:포트`.
    pub fn target(&self) -> HostPort {
        HostPort {
            host: self.host.clone(),
            port: self.port,
        }
    }

    /// 사람이 읽는 `호스트:포트`(진단·허용 목록 안내용). 경로·질의는 싣지 않는다.
    pub fn target_text(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// 서버 설치 판본. 승인에 묶이는 값이라 주소·접속 방식·자격 이름 중 하나라도 바뀌면 달라진다(옛 승인이 바뀐 목적지에 쓰이지 않는다).
    pub fn revision(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(b"bot-server-v1-remote\0");
        let port = self.port.to_string();
        let bearer = self.bearer.as_ref().map_or("", SecretName::as_str);
        for part in [
            if self.tls { "https" } else { "http" },
            self.host.as_str(),
            port.as_str(),
            self.path.as_str(),
            bearer,
        ] {
            field(&mut hash, part.as_bytes());
        }
        format!("{:x}", hash.finalize())
    }
}

/// `호스트[:포트]` 또는 `[IPv6][:포트]`를 나눈다(IPv6는 `:`가 든 호스트로 돌려주고 부르는 쪽이 거절한다).
fn split_authority(authority: &str) -> Result<(&str, Option<&str>), String> {
    if let Some(inner) = authority.strip_prefix('[') {
        let (ip, after) = inner.split_once(']').ok_or("url의 `[`가 닫히지 않았어요")?;
        return match after {
            "" => Ok((ip, None)),
            _ => after
                .strip_prefix(':')
                .map(|port| (ip, Some(port)))
                .ok_or_else(|| "url의 호스트 뒤에는 `:포트`만 올 수 있어요".to_owned()),
        };
    }
    Ok(match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    })
}

/// WHATWG URL의 «끝이 수인 호스트» 검사: 마지막 라벨(끝의 빈 라벨 하나는 건너뛴다)이 십진수이거나 `0x`로 시작하는 16진수(`0x`만 있어도)면 URL 파서가 IPv4로 읽는다.
fn ends_in_number(host: &str) -> bool {
    let mut parts: Vec<&str> = host.split('.').collect();
    if parts.last() == Some(&"") {
        if parts.len() == 1 {
            return false;
        }
        parts.pop();
    }
    let last = parts.last().copied().unwrap_or_default();
    let hex = last
        .strip_prefix("0x")
        .or_else(|| last.strip_prefix("0X"))
        .is_some_and(|rest| rest.bytes().all(|b| b.is_ascii_hexdigit()));
    !last.is_empty() && (last.bytes().all(|b| b.is_ascii_digit()) || hex)
}

/// 이름 호스트: 정규화할 수 있어야 하고 라벨이 `-`로 시작하거나 끝나지 않아야 한다(숫자로 끝나는 호스트는 부르기 전에 걸러진다).
fn named(host: &str) -> Result<String, String> {
    canon_host(host)
        .filter(|name| {
            name.split('.')
                .all(|label| !label.starts_with('-') && !label.ends_with('-'))
        })
        .ok_or_else(|| {
            "url의 호스트가 올바른 이름이 아니에요(소문자·숫자·`.`·`-`, 라벨이 `-`로 시작하거나 끝나면 안 돼요)"
                .to_owned()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(url: &str) -> Remote {
        Remote::parse(url, None).unwrap_or_else(|why| panic!("{url}: {why}"))
    }

    fn refused(url: &str) -> String {
        Remote::parse(url, None).expect_err(url)
    }

    #[test]
    fn https_names_and_loopback_http_literals_are_accepted() {
        let r = ok("https://API.Example.com/mcp?v=2");
        assert_eq!(
            (r.tls, r.host.as_str(), r.port, r.path.as_str()),
            (true, "api.example.com", 443, "/mcp?v=2")
        );
        assert_eq!(ok("https://mcp.example:8443").path, "/");
        assert_eq!(ok("https://mcp.example?x=1").path, "/?x=1");
        assert_eq!(ok("HTTPS://mcp.example./mcp").host, "mcp.example");
        let local = ok("http://127.0.0.1:9000/mcp");
        assert_eq!(
            (local.tls, local.host.as_str(), local.port),
            (false, "127.0.0.1", 9000)
        );
        assert_eq!(ok("http://127.9.9.9/").port, 80);
        // IPv6 리터럴은 허용 목록 문법에 쓸 수 없어 쓰지 않는다(아래 거절 목록)
        // https에 IP 리터럴을 써도 되지만 연결 때 공개 주소 검사를 받는다
        assert_eq!(ok("https://93.184.216.34/mcp").host, "93.184.216.34");
    }

    #[test]
    fn everything_else_is_refused() {
        for url in [
            "http://mcp.example/mcp",
            "http://localhost:9000/",
            "http://10.0.0.5/",
            "http://192.168.1.1/",
            "http://0.0.0.0/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::1]:7/m",
            "http://[::1]/",
            "https://[::1]/",
            "https://[2001:db8::1]:8443/mcp",
            "https://[::ffff:93.184.216.34]/",
            "http://2130706433/",
            "ftp://mcp.example/",
            "mcp.example/mcp",
            "https://user:pass@mcp.example/",
            "https://user@mcp.example/",
            "https://mcp.example/mcp#frag",
            "https://mcp.example:0/",
            "https://mcp.example:65536/",
            "https://mcp.example:44a/",
            "https://mcp.example:/",
            "https://127.1/",
            "https://2130706433/",
            "https://0x7f.1/",
            // 운영체제·URL 파서가 IPv4로 읽는 비정규 숫자 표기: 16진·8진·정수·축약·끝의 점
            "https://0x7f000001/",
            "https://0X7F000001/mcp",
            "https://0x7f.0x0.0x0.0x1/",
            "https://0177.0.0.1/",
            "https://0177.0.0.01/",
            "https://017700000001/",
            "https://127.0.1/",
            "https://127.0.0.1./mcp",
            "https://1.2.3.4./",
            "https://example.0x1/",
            "https://example.0/",
            "https://example.123/",
            "https://0x/",
            "http://0x7f000001/",
            "http://0177.0.0.1/",
            "http://127.1/",
            "https://a..b.example/",
            "https://-a.example/",
            "https://exämple.com/",
            "https://mcp.example/a b",
            "https://mcp.example/\n",
            "https://[::1/",
            "https://[::1]x/",
            "",
        ] {
            let why = refused(url);
            assert!(!why.is_empty(), "{url}");
        }
        assert!(refused(&format!("https://a.example/{}", "x".repeat(MAX_URL))).contains("2048"));
    }

    #[test]
    fn the_revision_binds_destination_scheme_path_and_credential_name() {
        let secret = |n: &str| SecretName::parse(n);
        let base = Remote::parse("https://mcp.example/mcp", secret("bearer")).unwrap();
        let same = Remote::parse("https://MCP.example:443/mcp", secret("bearer")).unwrap();
        assert_eq!(base.revision(), same.revision());
        for other in [
            Remote::parse("https://mcp.example/mcp", secret("other")),
            Remote::parse("https://mcp.example/mcp", None),
            Remote::parse("https://mcp.example/mcp2", secret("bearer")),
            Remote::parse("https://mcp.example:8443/mcp", secret("bearer")),
            Remote::parse("https://mcp2.example/mcp", secret("bearer")),
            Remote::parse("http://127.0.0.1/mcp", secret("bearer")),
        ] {
            assert_ne!(base.revision(), other.unwrap().revision());
        }
        assert_eq!(base.revision().len(), 64);
        assert_eq!(base.target_text(), "mcp.example:443");
    }
}
