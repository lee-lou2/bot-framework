use super::*;
use axum::http::Extensions;

fn with_peer(addr: &str) -> Extensions {
    let mut extensions = Extensions::new();
    extensions.insert(ConnectInfo(addr.parse::<SocketAddr>().unwrap()));
    extensions
}

#[test]
fn the_source_is_the_real_peer_ip_and_an_ipv4_mapped_address_is_the_same_source_as_the_ipv4_one() {
    let plain = peer(&with_peer("10.1.2.3:4000"));
    assert_eq!(plain, Some("10.1.2.3".parse().unwrap()));
    // 듀얼 스택 소켓은 IPv4 접속을 IPv4-매핑 IPv6로 보여 준다: 같은 출처다
    assert_eq!(peer(&with_peer("[::ffff:10.1.2.3]:4001")), plain);
    // 포트가 달라도 같은 출처다
    assert_eq!(peer(&with_peer("10.1.2.3:65000")), plain);
    // 진짜 IPv6는 그대로(다른 출처)
    assert_eq!(
        peer(&with_peer("[2001:db8::1]:1")),
        Some("2001:db8::1".parse().unwrap())
    );
}

#[test]
fn an_unknown_peer_has_no_source_and_request_headers_never_name_one() {
    assert_eq!(peer(&Extensions::new()), None);
    // 요청 머리는 `Extensions`에 없다: `X-Forwarded-For` 같은 값은 출처가 될 수 없다
    let request = axum::http::Request::builder()
        .header("x-forwarded-for", "203.0.113.9")
        .body(())
        .unwrap();
    assert_eq!(peer(request.extensions()), None);
}
