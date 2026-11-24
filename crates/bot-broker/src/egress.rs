//! egress 판정. 도구는 선언한 호스트:포트로만 나가고, 이름을 풀어 얻은 주소도 공개 주소여야 한다.
//!
//! - 허용 목록 = 도구가 선언한 호스트 ∩ 운영자가 프로필별로 정한 상한(상한이 없으면 선언 전부).
//! - 요청마다 정확한 호스트와 포트를 대조한다(와일드카드·접미사 일치 없음).
//! - 이름을 풀어 얻은 주소가 하나라도 사설·루프백·링크로컬 같은 비공개 주소면 그 이름 전체를 거절한다
//!   (DNS 재바인딩·로컬 네트워크 우회). 연결은 검사한 주소에 고정한다.
//!
//! 프록시 서버(`CONNECT` 처리)는 이 판정 위에서 돌아간다.

use crate::resolve::HostPort;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// 호스트 이름을 비교용으로 정규화한다. 소문자, 끝의 점 하나 제거. ASCII 영숫자·`.`·`-`만 허용한다
/// (IDN은 punycode로 선언해야 한다). 빈 라벨(`a..b`)과 `-`·`.`로 시작하는 이름은 거절한다.
pub fn canon_host(host: &str) -> Option<String> {
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    let ok = !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
        && host
            .split('.')
            .all(|label| !label.is_empty() && label.len() <= 63);
    ok.then_some(host)
}

/// `CONNECT` 요청 대상(`host:port`)을 나눈다. 대괄호 IPv6·사용자 정보·경로·공백은 거절한다.
pub fn parse_target(target: &str) -> Option<(String, u16)> {
    let (host, port) = target.rsplit_once(':')?;
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let port: u16 = port.parse().ok().filter(|p| *p != 0)?;
    Some((canon_host(host)?, port))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EgressPolicy {
    allowed: Vec<HostPort>,
}

impl EgressPolicy {
    /// 선언한 목록과 운영자 상한의 교집합. 정규화할 수 없는 선언은 버린다.
    pub fn new(declared: &[HostPort], cap: Option<&[HostPort]>) -> Self {
        let canon = |item: &HostPort| {
            canon_host(&item.host).map(|host| HostPort {
                host,
                port: item.port,
            })
        };
        let cap: Option<Vec<HostPort>> = cap.map(|c| c.iter().filter_map(canon).collect());
        let mut allowed: Vec<HostPort> = declared
            .iter()
            .filter_map(canon)
            .filter(|item| cap.as_ref().is_none_or(|c| c.contains(item)))
            .collect();
        allowed.sort();
        allowed.dedup();
        Self { allowed }
    }

    pub fn allowed(&self) -> &[HostPort] {
        &self.allowed
    }

    pub fn is_empty(&self) -> bool {
        self.allowed.is_empty()
    }

    /// 정확히 허용된 호스트와 포트인가.
    pub fn permits(&self, host: &str, port: u16) -> bool {
        canon_host(host).is_some_and(|host| {
            self.allowed
                .iter()
                .any(|item| item.host == host && item.port == port)
        })
    }
}

/// 공개 인터넷 주소인가. 루프백·사설·링크로컬·공유(CGNAT)·문서용·벤치마크·예약·멀티캐스트·미지정,
/// IPv4를 품은 IPv6 형태(매핑·NAT64·6to4)는 품은 IPv4로, Teredo는 거절한다.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => public_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => public_v4(v4),
            None => public_v6(v6),
        },
    }
}

fn public_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (64..=127).contains(&o[1]))
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        || o[0] >= 240)
}

fn public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return false;
    }
    if (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 || (s[0] & 0xffc0) == 0xfec0 {
        return false; // 고유 로컬·링크로컬·사이트로컬
    }
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return false; // 문서용
    }
    if s[0] == 0x2001 && s[1] == 0 {
        return false; // Teredo 터널
    }
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return public_v4(embedded(s[6], s[7])); // NAT64
    }
    if s[0] == 0x2002 {
        return public_v4(embedded(s[1], s[2])); // 6to4
    }
    s[0..6] != [0, 0, 0, 0, 0, 0] // IPv4 호환(::a.b.c.d, 폐기됨)
}

fn embedded(high: u16, low: u16) -> Ipv4Addr {
    let [a, b] = high.to_be_bytes();
    let [c, d] = low.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

/// 이름 해석 결과를 검사한다. 비어 있거나 비공개 주소가 하나라도 있으면 `None`.
/// 운영자가 사설망을 명시적으로 열었을 때(`allow_private`)만 비공개 주소를 허용한다.
/// 돌려주는 첫 주소에 연결을 고정한다.
pub fn vet_resolved(addrs: &[IpAddr], allow_private: bool) -> Option<IpAddr> {
    if addrs.is_empty() || (!allow_private && !addrs.iter().all(|ip| is_public(*ip))) {
        return None;
    }
    addrs.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hp(host: &str, port: u16) -> HostPort {
        HostPort {
            host: host.into(),
            port,
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn allow_list_is_declared_intersect_operator_cap() {
        let declared = [
            hp("api.example", 443),
            hp("files.example", 443),
            hp("api.example", 8443),
        ];
        let cap = [hp("API.example.", 443), hp("files.example", 80)];
        let policy = EgressPolicy::new(&declared, Some(&cap));
        assert_eq!(policy.allowed(), &[hp("api.example", 443)]);
        assert!(policy.permits("api.example", 443));
        assert!(policy.permits("API.EXAMPLE.", 443));
        assert!(!policy.permits("files.example", 443));
        assert!(!policy.permits("api.example", 8443));
    }

    #[test]
    fn without_a_cap_the_declaration_stands_and_nothing_else() {
        let policy = EgressPolicy::new(&[hp("api.example", 443)], None);
        assert!(policy.permits("api.example", 443));
        assert!(!policy.permits("evil.example", 443));
        assert!(!policy.permits("api.example", 80));
        assert!(EgressPolicy::new(&[], None).is_empty());
        assert!(!EgressPolicy::new(&[], None).permits("api.example", 443));
    }

    #[test]
    fn matching_is_exact_not_suffix_or_wildcard() {
        let policy = EgressPolicy::new(&[hp("api.example", 443)], None);
        for host in [
            "x.api.example",
            "api.example.evil",
            "evilapi.example",
            "*.example",
            "api.example@evil",
            "",
        ] {
            assert!(!policy.permits(host, 443), "{host}");
        }
    }

    #[test]
    fn unnormalizable_declarations_are_dropped() {
        let policy = EgressPolicy::new(
            &[hp("bad host", 443), hp("a..b", 443), hp("ok.example", 443)],
            None,
        );
        assert_eq!(policy.allowed(), &[hp("ok.example", 443)]);
    }

    #[test]
    fn parses_connect_targets_strictly() {
        assert_eq!(
            parse_target("API.example:443"),
            Some(("api.example".into(), 443))
        );
        for bad in [
            "api.example",
            "api.example:",
            "api.example:0",
            "api.example:65536",
            "api.example:44a",
            "[::1]:443",
            "user@api.example:443",
            "api.example/x:443",
            "api example:443",
            ":443",
            "api.example:-1",
        ] {
            assert_eq!(parse_target(bad), None, "{bad}");
        }
    }

    #[test]
    fn private_and_special_addresses_are_not_public() {
        for text in [
            "127.0.0.1",
            "127.255.255.254",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "0.1.2.3",
            "192.0.0.8",
            "192.0.2.1",
            "198.51.100.1",
            "203.0.113.9",
            "198.18.0.1",
            "198.19.255.255",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "2001:0:4136:e378:8000:63bf:3fff:fdd2",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b::a00:1",
            "2002:7f00:1::1",
            "2002:c0a8:101::",
            "::7f00:1",
        ] {
            assert!(!is_public(ip(text)), "{text}");
        }
    }

    #[test]
    fn ordinary_public_addresses_pass() {
        for text in [
            "93.184.216.34",
            "8.8.8.8",
            "1.1.1.1",
            "100.63.255.255",
            "100.128.0.1",
            "198.17.255.255",
            "198.20.0.1",
            "223.255.255.255",
            "2606:2800:220:1:248:1893:25c8:1946",
            "2a00:1450:4001::200e",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
        ] {
            assert!(is_public(ip(text)), "{text}");
        }
    }

    #[test]
    fn one_private_answer_rejects_the_whole_name() {
        let public = ip("93.184.216.34");
        assert_eq!(vet_resolved(&[public], false), Some(public));
        assert_eq!(vet_resolved(&[public, ip("10.0.0.5")], false), None); // 재바인딩
        assert_eq!(vet_resolved(&[ip("127.0.0.1")], false), None);
        assert_eq!(vet_resolved(&[], false), None);
        // 운영자가 사설망을 열었을 때만 허용하고 첫 주소에 고정한다
        assert_eq!(
            vet_resolved(&[ip("10.0.0.5"), public], true),
            Some(ip("10.0.0.5"))
        );
    }

    #[test]
    fn obfuscated_numeric_names_still_fail_on_the_resolved_address() {
        // 숫자만의 호스트 이름은 선언할 수 있지만(영숫자 규칙), 풀린 주소가 루프백이면 거절한다
        let policy = EgressPolicy::new(&[hp("2130706433", 443)], None);
        assert!(policy.permits("2130706433", 443));
        assert_eq!(vet_resolved(&[ip("127.0.0.1")], false), None);
    }
}
