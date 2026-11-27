//! Egress host allowlisting and public-IP vetting for tool HTTP/CONNECT proxies.
//!
//! - Effective allowlist = tool-declared hosts ∩ operator per-profile cap (or all declared if uncapped).
//! - Matching is exact on host and port (no wildcards or suffix matching).
//! - Any non-public resolved address rejects the entire hostname to prevent DNS rebinding and local network bypass; connections pin to the vetted address.

use crate::resolve::HostPort;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Normalizes a hostname for comparison: lowercase, strips one trailing dot, ASCII alphanumeric/`.`/`-` only (IDNs must use punycode).
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

/// Parses a `CONNECT` `host:port` target; rejects bracketed IPv6, userinfo, paths, and whitespace.
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
    /// Intersects declared targets with the optional operator cap, dropping unnormalizable entries.
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

    pub fn permits(&self, host: &str, port: u16) -> bool {
        canon_host(host).is_some_and(|host| {
            self.allowed
                .iter()
                .any(|item| item.host == host && item.port == port)
        })
    }
}

/// Returns true only for globally routable public IPs; inspects embedded IPv4 in IPv6 mapped/NAT64/6to4 addresses and rejects Teredo.
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
        return false; // ULA, link-local, site-local
    }
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return false; // documentation
    }
    if s[0] == 0x2001 && s[1] == 0 {
        return false; // Teredo
    }
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return public_v4(embedded(s[6], s[7])); // NAT64
    }
    if s[0] == 0x2002 {
        return public_v4(embedded(s[1], s[2])); // 6to4
    }
    s[0..6] != [0, 0, 0, 0, 0, 0] // deprecated IPv4-compatible (::a.b.c.d)
}

fn embedded(high: u16, low: u16) -> Ipv4Addr {
    let [a, b] = high.to_be_bytes();
    let [c, d] = low.to_be_bytes();
    Ipv4Addr::new(a, b, c, d)
}

/// Vets resolved addresses and returns the first address to pin the connection to.
/// Rejects empty results or any non-public address unless `allow_private` is set.
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
        assert_eq!(vet_resolved(&[public, ip("10.0.0.5")], false), None); // DNS rebinding
        assert_eq!(vet_resolved(&[ip("127.0.0.1")], false), None);
        assert_eq!(vet_resolved(&[], false), None);
        assert_eq!(
            vet_resolved(&[ip("10.0.0.5"), public], true),
            Some(ip("10.0.0.5"))
        );
    }

    #[test]
    fn obfuscated_numeric_names_still_fail_on_the_resolved_address() {
        // Numeric-only hostnames pass syntax validation but still fail when resolved to loopback.
        let policy = EgressPolicy::new(&[hp("2130706433", 443)], None);
        assert!(policy.permits("2130706433", 443));
        assert_eq!(vet_resolved(&[ip("127.0.0.1")], false), None);
    }
}
