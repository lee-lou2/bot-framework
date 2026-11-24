//! 진짜 Apple `container` CLI와 로컬 `alpine:3.20` 이미지로 확인하는 모델 VM 어댑터 카나리(이미지를 내려받지 않는다). 방화벽 init이 없는 이미지라
//! 호스트 전용 망만으로 되는 격리를 본다: 읽기 전용 입력/쓰기 scratch·읽기 전용 루트(+`/tmp`만 쓰기)·capability 제거·환경 값(비밀은 명령줄 밖)·
//! 외부로 가는 길 없음·릴레이 환경 고정·`--rm`로 끝난 컨테이너가 남지 않음·`reap`이 이 시도의 VM만 정리.
//! 방화벽이 없으면 같은 게이트웨이의 다른 호스트 포트가 열려 있다는 것도 여기서 드러낸다(그래서 운영은 신뢰 init을 요구한다. 그 경계는 `container_firewall.rs`).
//! `container`·서비스·이미지가 없으면 건너뛴다. `BOT_CONTAINER_CANARY=required`이면 실패한다.
//! 실행: `BOT_CONTAINER_CANARY=required cargo test -p bot-runtime --test container_canary -- --nocapture`

mod vm;

use std::time::Duration;
use vm::*;

#[tokio::test(flavor = "multi_thread")]
async fn a_real_vm_is_isolated_as_promised_and_reap_removes_only_this_attempts_vm() {
    let Some(program) = available() else {
        return;
    };
    let _serial = serial().await;
    let env = Env::new(program, "iso");
    let anchored = ContainerSandbox::start(env.config()).await.expect("시작");
    let gateway = anchored.address();
    // 호스트의 게이트웨이 서비스 둘(릴레이·다른 포트)과 loopback 서비스
    let (relay_port, relay_rx, _relay) = service(anchored.bind_addr(0));
    let (other_port, other_rx, _other) = service(anchored.bind_addr(0));
    let (loop_port, loop_rx, _loop) = service("127.0.0.1:0".parse().unwrap());
    let sandbox = anchored.relay(relay_port).await.expect("릴레이");
    // 어댑터가 릴레이를 확인하려고 연결한 흔적(빈 줄)을 비운다
    while relay_rx.try_recv().is_ok() {}

    let script = format!(
        r#"
echo "input=$(cat /bot/in.txt)"
echo "stdin=$(head -n1)"
(touch /bot/x 2>/dev/null && echo ro-input=WRITABLE) || echo ro-input=readonly
echo "scratch=$(echo hi > /work/out.txt && cat /work/out.txt)"
(touch /etc/canary 2>/dev/null && echo rootfs=WRITABLE) || echo rootfs=readonly
echo "tmp=$(echo t > /tmp/t && cat /tmp/t)"
(chown 12345 /work/out.txt 2>/dev/null && echo caps=KEPT) || echo caps=dropped
echo "env_plain=$PLAIN_VALUE"
echo "secret_len=${{#CANARY_API_TOKEN}}"
echo "home=$HOME"
echo "relay=$BOT_RELAY_IP:$BOT_RELAY_PORT"
echo "url=$ANTHROPIC_BASE_URL"
(wget -T3 -qO- http://1.1.1.1 >/dev/null 2>&1 && echo internet=REACHABLE) || echo internet=unreachable
(ping -c1 -W2 1.1.1.1 >/dev/null 2>&1 && echo ping=REACHABLE) || echo ping=unreachable
(echo from-vm | nc -w3 {gateway} {relay_port} && echo relay=reached) || echo relay=UNREACHABLE
(echo other | nc -w3 {gateway} {other_port} && echo other-gateway-port=open) || echo other-gateway-port=closed
(echo x | nc -w3 {gateway} {loop_port} && echo loopback-service=REACHABLE) || echo loopback-service=unreachable
echo done
"#
    );
    let env_vars = vec![
        ("PLAIN_VALUE".to_owned(), "plain-1".to_owned()),
        ("CANARY_API_TOKEN".to_owned(), "token-secret-123".to_owned()),
        ("HOME".to_owned(), "/work".to_owned()),
        // 호스트가 연 릴레이와 같은 주소만 받는다(다르면 띄우지 않는다)
        (
            "ANTHROPIC_BASE_URL".to_owned(),
            format!("http://{gateway}:{relay_port}"),
        ),
    ];
    let mut spec = spec(&env.root, "isolation", &script, env_vars.clone());
    let mut vm = sandbox.spawn(spec.clone()).await.expect("VM");
    let out = lines(&mut vm).await;
    let exit = vm.wait().await;
    let text = out.join("\n");
    for expected in [
        "input=읽기 전용 입력".to_owned(),
        "stdin=stdin body".into(),
        "ro-input=readonly".into(),
        "scratch=hi".into(),
        "rootfs=readonly".into(),
        "tmp=t".into(),
        "caps=dropped".into(),
        "env_plain=plain-1".into(),
        "secret_len=16".into(),
        "home=/work".into(),
        format!("relay={gateway}:{relay_port}"),
        format!("url=http://{gateway}:{relay_port}"),
        "internet=unreachable".into(),
        "ping=unreachable".into(),
        "relay=reached".into(),
        // 방화벽 init이 없으면 같은 게이트웨이의 다른 호스트 포트가 열려 있다: 운영이 신뢰 init을 요구하는 이유다
        "other-gateway-port=open".into(),
        "loopback-service=unreachable".into(),
        "done".into(),
    ] {
        assert!(out.contains(&expected), "{expected}\n{text}");
    }
    assert_eq!(exit.code, Some(0), "{text}");
    assert_eq!(
        relay_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        "from-vm",
        "호스트의 릴레이가 VM의 요청을 받았다"
    );
    assert_eq!(
        other_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
        "other"
    );
    assert!(
        loop_rx.try_recv().is_err(),
        "loopback 서비스는 받은 것이 없다"
    );
    // 쓰기는 scratch뿐이다: 입력 폴더에는 아무것도 만들어지지 않았고, scratch의 파일은 호스트에서 보인다
    assert_eq!(
        std::fs::read_to_string(env.root.join("work/out.txt"))
            .unwrap()
            .trim(),
        "hi"
    );
    assert!(!env.root.join("input/x").exists());

    // 호스트가 연 릴레이와 다른 주소를 환경으로 주면 띄우지 않는다
    spec.env[3].1 = "http://192.168.1.1:80".into();
    assert!(matches!(sandbox.spawn(spec).await, Err(Error::Invalid)));

    // `--rm`: 끝난 VM은 곧 사라지고 닻만 남는다
    env.until(15, "닻만 남아야 한다", |e| e.ids().len() == 1)
        .await;
    // 이 시도의 소유 표지로만 정리한다: 남의 표지는 그대로이고, 끝난 것을 확인해야 성공한다
    let owner = ContainerSandbox::owner_of(&Attempt {
        profile: ProfileId("canary".into()),
        run: RunId("other".into()),
        claim: Some(ClaimEpoch(1)),
        generation: Generation(1),
    });
    let mut other = sandbox
        .spawn(spec_sleep(&env, "other"))
        .await
        .expect("다른 시도의 VM");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(90), other.next_line())
            .await
            .unwrap()
            .as_deref(),
        Some("up")
    );
    sandbox
        .reap("canary.nobody.g1.c1")
        .await
        .expect("남의 표지");
    assert_eq!(env.ids().len(), 2, "닻과 이 시도의 VM");
    sandbox.reap(&owner).await.expect("reap");
    env.until(15, "이 시도의 VM만 사라진다", |e| {
        e.ids().len() == 1
    })
    .await;
    drop(other);
    // 종료는 닻과 이번 실행의 VM을 모두 치운다. 그 뒤 모델 VM은 거절된다
    sandbox.shutdown().await;
    assert!(env.ids().is_empty(), "{:?}", env.ids());
    assert!(matches!(
        sandbox.spawn(spec_sleep(&env, "late")).await,
        Err(Error::Unavailable)
    ));
}

fn spec_sleep(env: &Env, run: &str) -> ModelSpec {
    let mut spec = spec(
        &env.root,
        run,
        "echo up; sleep 60",
        vec![("ANTHROPIC_BASE_URL".to_owned(), String::new())],
    );
    spec.env.clear();
    spec.stdin = None;
    spec
}
