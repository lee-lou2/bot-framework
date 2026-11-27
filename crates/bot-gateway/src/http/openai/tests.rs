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
    // Dual-stack sockets report IPv4 peers as IPv4-mapped IPv6; normalize to the same source.
    assert_eq!(peer(&with_peer("[::ffff:10.1.2.3]:4001")), plain);
    assert_eq!(peer(&with_peer("10.1.2.3:65000")), plain);
    assert_eq!(
        peer(&with_peer("[2001:db8::1]:1")),
        Some("2001:db8::1".parse().unwrap())
    );
}

#[test]
fn an_unknown_peer_has_no_source_and_request_headers_never_name_one() {
    assert_eq!(peer(&Extensions::new()), None);
    let request = axum::http::Request::builder()
        .header("x-forwarded-for", "203.0.113.9")
        .body(())
        .unwrap();
    assert_eq!(peer(request.extensions()), None);
}
