//! 상류 주소 규칙: `https`만, `http`는 loopback IP 리터럴만, 자격·쿼리·조각·이름 속임수는 거절.

use super::super::ApiEndpoint;

fn ok(text: &str) -> ApiEndpoint {
    ApiEndpoint::parse(text).unwrap_or_else(|e| panic!("{text}: {e}"))
}

#[test]
fn https_hosts_ports_and_paths_are_normalized_into_one_base_url() {
    assert_eq!(
        ok("https://api.anthropic.com").url("/v1/messages"),
        "https://api.anthropic.com:443/v1/messages"
    );
    // 호스트는 소문자로, 끝 슬래시는 하나만 떼고, 기본 경로를 이어 붙인다
    assert_eq!(
        ok("https://API.Example.com:8443/v1/").url("/chat/completions"),
        "https://api.example.com:8443/v1/chat/completions"
    );
    assert_eq!(
        ok("https://gateway.example/anthropic/proxy").url("/v1/messages"),
        "https://gateway.example:443/anthropic/proxy/v1/messages"
    );
    // https의 공개 IP 리터럴은 허용하지만 키가 필요하다(loopback이 아니다)
    for text in [
        "https://api.example.com",
        "https://203.0.113.7:9443/v1",
        "https://[2001:db8::1]:443/v1",
    ] {
        assert!(!ok(text).is_loopback(), "{text}");
    }
    assert_eq!(
        ok("https://[2001:db8::1]:8443/v1").url("/chat/completions"),
        "https://[2001:db8::1]:8443/v1/chat/completions"
    );
}

#[test]
fn http_is_accepted_only_for_loopback_ip_literals() {
    for good in [
        "http://127.0.0.1:11434/v1",
        "http://127.9.9.9",
        "http://[::1]:8080/v1",
        "https://127.0.0.1:9443",
    ] {
        assert!(ok(good).is_loopback(), "{good}");
    }
    assert_eq!(
        ok("http://[::1]:8080/v1").url("/chat/completions"),
        "http://[::1]:8080/v1/chat/completions"
    );
    assert_eq!(ok("http://127.0.0.1/v1").url(""), "http://127.0.0.1:80/v1");
    // 이름(localhost 포함)·사설망·링크 로컬·미지정·IPv4로 매핑한 IPv6·IP의 변형 표기는 평문으로 보내지 않는다
    for bad in [
        "http://localhost:11434/v1",
        "http://LOCALHOST",
        "http://example.com",
        "http://10.0.0.5:8000/v1",
        "http://192.168.1.2",
        "http://169.254.169.254/latest",
        "http://0.0.0.0:80",
        "http://[::ffff:127.0.0.1]:80",
        "http://[::2]/",
        "http://127.0.0.1.nip.io",
        "http://2130706433",
        "http://127.1",
        "http://0x7f.0.0.1",
        "http://0x7f000001",
        "http://127.0x1",
        "http://0177.0.0.1",
    ] {
        assert!(ApiEndpoint::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn credentials_queries_fragments_escapes_and_odd_names_are_refused_without_echoing_the_input() {
    let long = format!("https://example.com/{}", "a".repeat(2100));
    for bad in [
        "",
        "ftp://example.com",
        "example.com",
        "https://",
        "https:///v1",
        "https://user@example.com",
        "https://user:pass@example.com",
        "https://example.com?x=1",
        "https://example.com/v1?key=abc",
        "https://example.com/v1#frag",
        "https://exa mple.com",
        "https://example.com/a b",
        "https://example.com/%2e%2e/x",
        "https://example.com/../x",
        "https://example.com/./x",
        "https://example.com//x",
        "https://example.com:0",
        "https://example.com:65536",
        "https://example.com:abc",
        "https://example.com:",
        "https://-bad.example.com",
        "https://bad-.example.com",
        "https://exa_mple.com",
        "https://example..com",
        "https://example.com.",
        "https://bücher.example",
        "https://example.com\\x",
        "https://example.com/v1\n",
        "https://[::1",
        "https://[::1]x",
        "https://::1",
        "https://[example.com]/",
        "https://1.2.3",
        "https://0x7f000001",
        "https://0X7F000001",
        "https://127.0x1",
        "https://example.0x1",
        "https://0177.0.0.1",
        "HTTPS://example.com",
        long.as_str(),
    ] {
        let error = ApiEndpoint::parse(bad).expect_err(bad);
        // 거절 사유는 고정 문장이다: 입력(자격이 섞여 있을 수 있다)을 되풀이하지 않는다
        assert!(
            !error.contains("example") && !error.contains("pass"),
            "{error}"
        );
    }
}
