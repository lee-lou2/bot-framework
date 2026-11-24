use super::*;
use crate::{
    resolve::HostPort,
    testing::{FakeConnector, FakeResolver},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const USER: &str = "bot";
const PASS: &str = "s3cret-pass";

fn policy(entries: &[(&str, u16)]) -> EgressPolicy {
    let declared: Vec<HostPort> = entries
        .iter()
        .map(|(host, port)| HostPort {
            host: host.to_string(),
            port: *port,
        })
        .collect();
    EgressPolicy::new(&declared, None)
}

struct Rig {
    proxy: Proxy,
    resolver: Arc<FakeResolver>,
    connector: Arc<FakeConnector>,
}

async fn rig(
    policy: EgressPolicy,
    dns: &[(&str, &[&str])],
    tweak: impl FnOnce(&mut ProxyConfig),
) -> Rig {
    let resolver = Arc::new(FakeResolver::new(dns));
    let connector = Arc::new(FakeConnector::new());
    let mut cfg = ProxyConfig::new(policy, USER.into(), PASS.into());
    tweak(&mut cfg);
    let proxy = Proxy::start(cfg, resolver.clone(), connector.clone())
        .await
        .unwrap();
    Rig {
        proxy,
        resolver,
        connector,
    }
}

fn basic(user: &str, pass: &str) -> String {
    format!(
        "Basic {}",
        base64_encode(format!("{user}:{pass}").as_bytes())
    )
}

/// 요청을 보내고 상태 줄(`HTTP/1.1 200 ...`)과 연결을 돌려준다.
async fn request(addr: SocketAddr, raw: &str) -> (String, TcpStream) {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut seen = Vec::new();
    let mut byte = [0u8; 1];
    while !seen.ends_with(b"\r\n\r\n") {
        match timeout(Duration::from_secs(5), stream.read(&mut byte)).await {
            Ok(Ok(1)) => seen.push(byte[0]),
            _ => break,
        }
    }
    let text = String::from_utf8_lossy(&seen).into_owned();
    (text.lines().next().unwrap_or("").to_owned(), stream)
}

fn connect_request(target: &str, auth: Option<&str>) -> String {
    let header = auth
        .map(|a| format!("Proxy-Authorization: {a}\r\n"))
        .unwrap_or_default();
    format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{header}\r\n")
}

#[tokio::test]
async fn a_declared_host_gets_a_tunnel_pinned_to_the_resolved_public_address() {
    let r = rig(
        policy(&[("api.example", 443)]),
        &[("api.example", &["93.184.216.34"])],
        |_| {},
    )
    .await;
    let (status, mut tunnel) = request(
        r.proxy.addr(),
        &connect_request("api.example:443", Some(&basic(USER, PASS))),
    )
    .await;
    assert!(status.starts_with("HTTP/1.1 200"), "{status}");
    tunnel.write_all(b"ping").await.unwrap();
    let mut back = [0u8; 4];
    timeout(Duration::from_secs(5), tunnel.read_exact(&mut back))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&back, b"ping");
    assert_eq!(
        r.connector.addrs(),
        vec!["93.184.216.34:443".parse().unwrap()]
    );
    assert_eq!(r.proxy.stats().tunnels, 1);
}

#[tokio::test]
async fn bytes_pipelined_right_after_the_head_are_forwarded() {
    let r = rig(
        policy(&[("api.example", 443)]),
        &[("api.example", &["93.184.216.34"])],
        |_| {},
    )
    .await;
    let raw = format!(
        "{}hello",
        connect_request("api.example:443", Some(&basic(USER, PASS)))
    );
    let (status, mut tunnel) = request(r.proxy.addr(), &raw).await;
    assert!(status.contains("200"));
    let mut back = [0u8; 5];
    timeout(Duration::from_secs(5), tunnel.read_exact(&mut back))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&back, b"hello");
}

#[tokio::test]
async fn missing_or_wrong_credentials_never_reach_resolution_or_connection() {
    let r = rig(
        policy(&[("api.example", 443)]),
        &[("api.example", &["93.184.216.34"])],
        |_| {},
    )
    .await;
    let cases = [
        None,
        Some(basic(USER, "wrong")),
        Some(basic("other", PASS)),
        Some(format!("Basic {}", base64_encode(PASS.as_bytes()))),
        Some("Bearer abc".to_owned()),
        Some("Basic !!!not-base64".to_owned()),
        Some(basic(USER, &format!("{PASS}x"))),
    ];
    for case in &cases {
        let (status, _) = request(
            r.proxy.addr(),
            &connect_request("api.example:443", case.as_deref()),
        )
        .await;
        assert!(status.contains("407"), "{case:?}: {status}");
    }
    assert!(r.resolver.calls().is_empty() && r.connector.addrs().is_empty());
    assert_eq!(r.proxy.stats().denied_auth, cases.len() as u64);
}

#[tokio::test]
async fn anything_not_declared_exactly_is_forbidden_before_resolution() {
    let r = rig(
        policy(&[("api.example", 443)]),
        &[("api.example", &["93.184.216.34"])],
        |_| {},
    )
    .await;
    let auth = basic(USER, PASS);
    for target in [
        "other.example:443",
        "x.api.example:443",
        "api.example:80",
        "api.example:8443",
        "93.184.216.34:443",
        "API.EXAMPLE.evil:443",
        "api.example.:80",
    ] {
        let (status, _) = request(r.proxy.addr(), &connect_request(target, Some(&auth))).await;
        assert!(status.contains("403"), "{target}: {status}");
    }
    assert!(r.resolver.calls().is_empty() && r.connector.addrs().is_empty());
    assert_eq!(r.proxy.stats().denied_policy, 7);
    // 대소문자와 끝의 점은 같은 호스트로 본다
    let (status, _) = request(
        r.proxy.addr(),
        &connect_request("API.example.:443", Some(&auth)),
    )
    .await;
    assert!(status.contains("200"), "{status}");
}

#[tokio::test]
async fn a_name_resolving_to_a_non_public_address_is_refused_even_when_declared() {
    let dns: &[(&str, &[&str])] = &[
        ("loopback.example", &["127.0.0.1"]),
        ("mixed.example", &["8.8.8.8", "10.0.0.5"]),
        ("metadata.example", &["169.254.169.254"]),
        ("v6.example", &["::1"]),
        ("numeric.example", &["::ffff:192.168.1.1"]),
    ];
    let entries: Vec<(&str, u16)> = dns.iter().map(|(h, _)| (*h, 443)).collect();
    let r = rig(policy(&entries), dns, |_| {}).await;
    let auth = basic(USER, PASS);
    for (host, _) in dns {
        let (status, _) = request(
            r.proxy.addr(),
            &connect_request(&format!("{host}:443"), Some(&auth)),
        )
        .await;
        assert!(status.contains("403"), "{host}: {status}");
    }
    assert!(
        r.connector.addrs().is_empty(),
        "비공개 주소로는 연결을 열지 않는다"
    );
    assert_eq!(r.proxy.stats().denied_address, dns.len() as u64);
}

#[tokio::test]
async fn an_operator_can_open_a_private_network_explicitly() {
    let r = rig(
        policy(&[("intranet.example", 443)]),
        &[("intranet.example", &["10.1.2.3"])],
        |c| c.allow_private = true,
    )
    .await;
    let (status, _) = request(
        r.proxy.addr(),
        &connect_request("intranet.example:443", Some(&basic(USER, PASS))),
    )
    .await;
    assert!(status.contains("200"));
    assert_eq!(r.connector.addrs(), vec!["10.1.2.3:443".parse().unwrap()]);
}

#[tokio::test]
async fn an_unresolvable_declared_name_is_a_bad_gateway() {
    let r = rig(policy(&[("gone.example", 443)]), &[], |_| {}).await;
    let (status, _) = request(
        r.proxy.addr(),
        &connect_request("gone.example:443", Some(&basic(USER, PASS))),
    )
    .await;
    assert!(status.contains("502"), "{status}");
    assert_eq!(r.proxy.stats().failed, 1);
}

#[tokio::test]
async fn only_connect_is_served_and_malformed_heads_are_rejected() {
    let r = rig(
        policy(&[("api.example", 443)]),
        &[("api.example", &["93.184.216.34"])],
        |_| {},
    )
    .await;
    let auth = basic(USER, PASS);
    let (status, _) = request(
        r.proxy.addr(),
        &format!("GET http://api.example/ HTTP/1.1\r\nProxy-Authorization: {auth}\r\n\r\n"),
    )
    .await;
    assert!(
        status.contains("405"),
        "평문 프록시는 열지 않는다: {status}"
    );
    for raw in [
        "garbage\r\n\r\n".to_owned(),
        "CONNECT api.example:443\r\n\r\n".to_owned(),
        format!("CONNECT api.example:443 HTTP/2\r\nProxy-Authorization: {auth}\r\n\r\n"),
        format!(
            "CONNECT api.example:443 HTTP/1.1\r\nProxy-Authorization: {auth}\r\nProxy-Authorization: {auth}\r\n\r\n"
        ),
        format!("CONNECT [::1]:443 HTTP/1.1\r\nProxy-Authorization: {auth}\r\n\r\n"),
        format!(
            "CONNECT api.example:443 HTTP/1.1\r\nX: {}\r\n\r\n",
            "a".repeat(9000)
        ),
    ] {
        let (status, _) = request(r.proxy.addr(), &raw).await;
        assert!(status.contains("400"), "{raw:.60}: {status}");
    }
    assert!(r.connector.addrs().is_empty());
}

#[tokio::test]
async fn stopping_the_proxy_cuts_open_tunnels_and_closes_the_port() {
    let r = rig(
        policy(&[("api.example", 443)]),
        &[("api.example", &["93.184.216.34"])],
        |_| {},
    )
    .await;
    let addr = r.proxy.addr();
    let (status, mut tunnel) = request(
        addr,
        &connect_request("api.example:443", Some(&basic(USER, PASS))),
    )
    .await;
    assert!(status.contains("200"));
    r.proxy.stop().await;
    let mut rest = Vec::new();
    let closed = timeout(Duration::from_secs(5), tunnel.read_to_end(&mut rest)).await;
    assert!(
        matches!(closed, Ok(Ok(_)) | Ok(Err(_))),
        "열린 터널이 끊겨야 한다"
    );
    assert!(TcpStream::connect(addr).await.is_err(), "포트도 닫힌다");
}

#[test]
fn base64_round_trips_and_refuses_malformed_input() {
    for len in 0..12usize {
        let data: Vec<u8> = (0..len as u8)
            .map(|b| b.wrapping_mul(37).wrapping_add(11))
            .collect();
        assert_eq!(base64_decode(&base64_encode(&data)).unwrap(), data);
    }
    assert_eq!(base64_encode(b"bot:pw"), "Ym90OnB3");
    for bad in ["abc", "ab=c", "====", "a===", "Zm9v!A==", "Zg==Zg=="] {
        assert!(base64_decode(bad).is_none(), "{bad}");
    }
    assert!(
        constant_time_eq(b"abc", b"abc")
            && !constant_time_eq(b"abc", b"abd")
            && !constant_time_eq(b"abc", b"ab")
    );
}
