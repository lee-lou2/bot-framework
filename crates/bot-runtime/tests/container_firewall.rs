//! 진짜 모델 이미지(신뢰 init 포함)로 확인하는 VM 안 방화벽 경계. `BOT_MODEL_IMAGE=<이름@sha256:…>`(`images/model/build.sh`가 알려 주는 정확한 참조)가 있어야 돈다.
//! 모델 VM은 이미지의 ENTRYPOINT(`bot-init`)가 root로 시작해 릴레이(IP:포트 TCP) 하나로만 나가는 nft 규칙을 걸고 capability를 모두 버린 뒤 비root로 고정 `claude`를 exec한다.
//! 여기서는 그 고정 `claude` 자리(`/usr/local/bin`)에 점검 스크립트를 덮어(init은 이미지의 것을 그대로 복사한다) «init이 끝난 뒤 모델이 보는 세계»를 잰다:
//! 릴레이만 닿는다 · 같은 게이트웨이의 다른 호스트 포트와 같은 망의 이웃 VM(닻 포함)과 바깥은 막힌다 · 규칙을 바꾸려는 시도는 모두 거절된다 · 호스트의 0700 쓰기 폴더에 쓴다.
//! init이 규칙을 걸지 못하면 모델은 시작하지 않는다.
//! 실행: `BOT_CONTAINER_CANARY=required BOT_MODEL_IMAGE=… cargo test -p bot-runtime --test container_firewall -- --nocapture`

mod vm;

use std::{
    net::{SocketAddr, TcpStream},
    os::unix::fs::PermissionsExt,
    process::Command,
    time::Duration,
};
use vm::*;

/// 닻이나 이웃이 듣는(듣지 않는) 포트.
const NEIGHBOR_PORT: u16 = 7000;

/// 점검 스크립트(자리표시자는 시험이 채운다). 막힌 점검은 시간 제한까지 기다리므로 나란히 돌린다.
/// `connected`는 연결됐다, `refused`는 상대가 바로 거절했다(패킷이 닿았다), `dropped`는 시간 제한까지 아무 답이 없었다(중간에서 버려졌다).
const PROBE: &str = r#"#!/bin/busybox sh
B=/bin/busybox
try() {
  s=$($B date +%s)
  if echo "probe-$1" | $B nc -w 3 "$2" "$3" >/dev/null 2>&1; then echo "$1=connected"
  else
    d=$(( $($B date +%s) - s ))
    if [ "$d" -ge 2 ]; then echo "$1=dropped"; else echo "$1=refused"; fi
  fi
}
probes() {
  try "relay$1" @GATEWAY@ @RELAY@ >/tmp/p1 &
  try "other-port$1" @GATEWAY@ @OTHER@ >/tmp/p2 &
  try "neighbor$1" @NEIGHBOR@ @NEIGHBOR_PORT@ >/tmp/p3 &
  try "anchor$1" @ANCHOR@ @NEIGHBOR_PORT@ >/tmp/p4 &
  try "external-443$1" 1.1.1.1 443 >/tmp/p5 &
  try "external-dns$1" 8.8.8.8 53 >/tmp/p6 &
  try "wildcard$1" @GATEWAY@ @WILD@ >/tmp/p7 &
  wait
  $B cat /tmp/p1 /tmp/p2 /tmp/p3 /tmp/p4 /tmp/p5 /tmp/p6 /tmp/p7
}
echo probe-started
echo "ids=$($B id -u):$($B id -g) groups=$($B id -G)"
$B grep -E '^(CapInh|CapPrm|CapEff|CapBnd|CapAmb|NoNewPrivs):' /proc/self/status | $B tr -d '\t' | $B sed 's/:/=/'
echo "url=$ANTHROPIC_BASE_URL"
probes ""
n=0
for cmd in '/usr/sbin/nft flush ruleset' '/usr/sbin/nft list ruleset' '/bin/setpriv --reuid=0 -- /bin/busybox id' \
  "$B ip addr add 10.99.99.9/32 dev lo" "$B ip link set lo down" 'echo 1 > /proc/sys/net/ipv4/ip_forward'; do
  n=$((n + 1))
  if /bin/sh -c "$cmd" >/dev/null 2>&1; then echo "tamper-$n=ALLOWED"; else echo "tamper-$n=denied"; fi
done
if $B ip -o addr show lo >/dev/null 2>&1; then echo control-ip=ok; else echo control-ip=missing; fi
probes "-after"
if $B sh -c 'echo from-10001 > /work/probe.txt'; then echo work=written; else echo work=FAILED; fi
echo done
"#;

/// 이미지의 init을 그대로 복사하고 그 곁에 점검 스크립트를 `claude`로 둔 폴더(`/usr/local/bin` 자리에 읽기 전용으로 덮는다).
fn overlay(env: &Env, image: &str, probe: &str) {
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let copied = ok(
        &env.program,
        &[
            "run",
            "--rm",
            "--name",
            &format!("{}-extract", env.instance),
            // 기본(NAT) 망의 VM을 띄우고 내리면 호스트 전용 망의 호스트 쪽 주소가 잠시 사라진다(vmnet): 이 시험의 망에서 돌린다
            "--network",
            &env.network,
            "--entrypoint",
            "/bin/busybox",
            "--mount",
            &format!("type=bind,source={},target=/out", bin.display()),
            image,
            "cp",
            "/usr/local/bin/bot-init",
            "/out/bot-init",
        ],
    );
    assert!(copied, "이미지의 bot-init을 복사하지 못했다");
    std::fs::write(bin.join("claude"), probe).unwrap();
    for name in ["bot-init", "claude"] {
        std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// 점검용 모델 명세: 고정 프로그램 이름 `claude`(init이 받는다)와 읽기 전용 입력·덮어쓴 `/usr/local/bin`·쓰기 `/work`.
fn model_spec(env: &Env, run: &str) -> ModelSpec {
    ModelSpec {
        attempt: Attempt {
            profile: ProfileId("canary".into()),
            run: RunId(run.into()),
            claim: Some(ClaimEpoch(1)),
            generation: Generation(1),
        },
        program: "claude".into(),
        args: Vec::new(),
        env: Vec::new(),
        cwd: "/bot".into(),
        readonly_mounts: vec![
            Mount {
                host: env.root.join("input"),
                guest: "/bot".into(),
            },
            Mount {
                host: env.root.join("bin"),
                guest: "/usr/local/bin".into(),
            },
        ],
        scratch: Some(Mount {
            host: env.root.join("work"),
            guest: "/work".into(),
        }),
        stdin: None,
        timeout: Duration::from_secs(90),
        idle_timeout: Duration::from_secs(60),
    }
}

fn reaches(address: &str, port: u16) -> std::io::Result<()> {
    let target: SocketAddr = format!("{address}:{port}").parse().unwrap();
    TcpStream::connect_timeout(&target, Duration::from_secs(2)).map(drop)
}

#[tokio::test(flavor = "multi_thread")]
async fn after_init_the_model_reaches_only_the_relay_and_cannot_change_the_rules() {
    let Some(program) = available() else {
        return;
    };
    let _serial = serial().await;
    let Some(image) = model_image(&program) else {
        return;
    };
    let env = Env::new(program.clone(), "fw");
    let anchored = ContainerSandbox::start(env.firewalled(&image))
        .await
        .expect("시작");
    let gateway = anchored.address().to_string();
    let (relay_port, relay_rx, _relay) = service(anchored.bind_addr(0));
    let (other_port, other_rx, _other) = service(anchored.bind_addr(0));
    // 모든 인터페이스(0.0.0.0)에 연 호스트 서비스는 게이트웨이 주소로도 닿는다: 호스트 전용 망만으로는 막히지 않는 길이다
    let (wild_port, wild_rx, _wild) = service("0.0.0.0:0".parse().unwrap());
    let sandbox = anchored.relay(relay_port).await.expect("릴레이");
    while relay_rx.try_recv().is_ok() {}

    // 같은 호스트 전용 망의 이웃 VM(듣고 있다)과 닻(듣지 않는다): 호스트에서는 둘 다 닿는다(이웃은 연결, 닻은 즉시 거절)
    let neighbor = format!("{}-neighbor", env.instance);
    assert!(ok(
        &program,
        &[
            "run",
            "-d",
            "--rm",
            "--name",
            &neighbor,
            "--network",
            &env.network,
            IMAGE,
            "sh",
            "-c",
            &format!("while true; do nc -l -p {NEIGHBOR_PORT} >/dev/null; done"),
        ]
    ));
    let mut neighbor_ip = None;
    for _ in 0..200 {
        neighbor_ip = ip_of(&program, &neighbor).filter(|ip| reaches(ip, NEIGHBOR_PORT).is_ok());
        if neighbor_ip.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let neighbor_ip = neighbor_ip.expect("이웃 VM이 호스트에서 닿아야 한다(대조)");
    let anchor_ip = ip_of(&program, &running_anchor(&program, &env.instance).await).unwrap();
    // 막 뜬 VM은 첫 연결이 늦을 수 있다: 닫힌 포트의 «즉시 거절»이 올 때까지 잠깐 다시 해 본다
    let mut kind = None;
    for _ in 0..40 {
        kind = reaches(&anchor_ip, NEIGHBOR_PORT).err().map(|e| e.kind());
        if kind == Some(std::io::ErrorKind::ConnectionRefused) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        kind,
        Some(std::io::ErrorKind::ConnectionRefused),
        "닻은 호스트에서 닿고 그 포트는 닫혀 있다(대조)"
    );

    let probe = PROBE
        .replace("@GATEWAY@", &gateway)
        .replace("@RELAY@", &relay_port.to_string())
        .replace("@OTHER@", &other_port.to_string())
        .replace("@WILD@", &wild_port.to_string())
        .replace("@NEIGHBOR@", &neighbor_ip)
        .replace("@ANCHOR@", &anchor_ip)
        .replace("@NEIGHBOR_PORT@", &NEIGHBOR_PORT.to_string());
    overlay(&env, &image, &probe);

    // 대조: init 없이(`--entrypoint`로 점검 스크립트를 바로) 같은 망에서 같은 점검을 돌리면 릴레이·다른 호스트 포트·이웃에 닿고 닻은 즉시 거절한다.
    // 아래의 «버려진다»가 init이 건 규칙 때문이지 시험 환경 때문이 아님을 보인다. 막 만든 망의 첫 VM은 연결이 늦게 열릴 수 있어(환경 사정) 대조만 몇 번 다시 한다.
    let mut open = String::new();
    for attempt in 0..3 {
        let run = Command::new(&program)
            .args([
                "run",
                "--rm",
                "--name",
                &format!("{}-control{attempt}", env.instance),
            ])
            .args([
                "--network",
                &env.network,
                "--entrypoint",
                "/usr/local/bin/claude",
            ])
            .args([
                "--mount",
                &format!(
                    "type=bind,source={},target=/usr/local/bin,readonly",
                    env.root.join("bin").display()
                ),
            ])
            .arg(&image)
            .output()
            .expect("대조 VM");
        open = String::from_utf8_lossy(&run.stdout).into_owned();
        let open_world = [
            "relay=connected",
            "other-port=connected",
            "wildcard=connected",
            "neighbor=connected",
            "anchor=refused",
        ];
        if open_world
            .iter()
            .all(|line| open.lines().any(|l| l == *line))
        {
            break;
        }
    }
    for line in [
        "relay=connected",
        "other-port=connected",
        "wildcard=connected",
        "neighbor=connected",
        "anchor=refused",
    ] {
        assert!(open.lines().any(|l| l == line), "대조: {line}\n{open}");
    }
    let reached: Vec<String> =
        std::iter::from_fn(|| other_rx.recv_timeout(Duration::from_millis(800)).ok()).collect();
    assert!(
        reached.iter().any(|m| m == "probe-other-port"),
        "대조 VM의 요청이 다른 호스트 포트에 닿아야 한다: {reached:?}"
    );
    while relay_rx.try_recv().is_ok() {}
    // 대조 VM의 요청은 0.0.0.0 서비스에도 닿았다(그 길이 열려 있다는 증거). 이후 방화벽 아래의 요청과 섞이지 않게 비운다
    let wild_reached: Vec<String> =
        std::iter::from_fn(|| wild_rx.recv_timeout(Duration::from_millis(800)).ok()).collect();
    assert!(
        wild_reached.iter().any(|m| m == "probe-wildcard"),
        "대조 VM의 요청이 0.0.0.0 서비스에 닿아야 한다: {wild_reached:?}"
    );

    let mut vm = sandbox
        .spawn(model_spec(&env, "firewall"))
        .await
        .expect("VM");
    let out = lines(&mut vm).await;
    let exit = vm.wait().await;
    let text = format!("{}\n--- stderr: {}", out.join("\n"), exit.stderr_tail);
    eprintln!("{text}");
    let expect = |line: &str| assert!(out.iter().any(|l| l == line), "{line}\n{text}");
    expect("probe-started");
    // init이 끝난 뒤의 모델: 비root·보조 그룹 없음·capability 전부 0·no_new_privs
    expect("ids=10001:10001 groups=10001");
    for set in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        expect(&format!("{set}=0000000000000000"));
    }
    expect("NoNewPrivs=1");
    expect(&format!("url=http://{gateway}:{relay_port}"));
    // 릴레이만 닿는다. 같은 게이트웨이의 다른 호스트 포트와 이웃·닻은 «버려진다»(시간 제한까지 무응답): 거절이 아니므로 패킷이 나가지도 못했다
    for after in ["", "-after"] {
        expect(&format!("relay{after}=connected"));
        for blocked in ["other-port", "wildcard", "neighbor", "anchor"] {
            expect(&format!("{blocked}{after}=dropped"));
        }
        for outside in ["external-443", "external-dns"] {
            assert!(
                !out.iter()
                    .any(|l| *l == format!("{outside}{after}=connected")),
                "{outside}{after}\n{text}"
            );
        }
    }
    // 규칙을 바꾸려는 시도(nft·setpriv·ip·sysctl)는 모두 거절되고, 시도 뒤에도 규칙은 그대로다(위 `-after`)
    for n in 1..=6 {
        expect(&format!("tamper-{n}=denied"));
    }
    expect("control-ip=ok");
    // 호스트의 0700 쓰기 폴더에 비root가 쓰고, 호스트는 그 파일을 읽고 치운다
    expect("work=written");
    expect("done");
    assert_eq!(exit.code, Some(0), "{text}");
    let written = env.root.join("work/probe.txt");
    assert_eq!(
        std::fs::read_to_string(&written).unwrap().trim(),
        "from-10001"
    );
    std::fs::remove_file(&written).expect("호스트가 게스트가 만든 파일을 치운다");
    // 호스트의 서비스: 릴레이는 VM의 요청을 받았고 다른 포트는 아무것도 받지 못했다
    let got: Vec<String> =
        std::iter::from_fn(|| relay_rx.recv_timeout(Duration::from_secs(2)).ok()).collect();
    assert!(got.iter().all(|m| m.starts_with("probe-relay")), "{got:?}");
    assert!(!got.is_empty(), "릴레이가 받은 것이 없다");
    assert!(
        other_rx.try_recv().is_err(),
        "다른 호스트 포트가 받은 것이 있다"
    );
    // 0.0.0.0 서비스에는 방화벽 아래의 모델 VM의 요청이 하나도 닿지 않았다(대조 VM의 요청은 위에서 비웠다)
    assert!(
        wild_rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "방화벽 아래에서 0.0.0.0 서비스에 닿았다"
    );

    sandbox.shutdown().await;
    ok(&program, &["kill", &neighbor]);
    ok(&program, &["delete", "--force", &neighbor]);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_init_that_cannot_apply_the_rules_never_starts_the_model() {
    let Some(program) = available() else {
        return;
    };
    let _serial = serial().await;
    let Some(image) = model_image(&program) else {
        return;
    };
    let env = Env::new(program, "fwfail");
    let mut config = env.firewalled(&image);
    // 방화벽을 걸 권한(NET_ADMIN)이 없다: init이 규칙을 걸지 못하고 «모델을 시작하지 않은 채» 비정상 종료해야 한다
    config.entry = Some(Entry {
        model_caps: ["SETGID", "SETPCAP", "SETUID"].map(String::from).into(),
        ..Entry::model_image()
    });
    let anchored = ContainerSandbox::start(config).await.expect("시작");
    let (relay_port, _rx, _relay) = service(anchored.bind_addr(0));
    let sandbox = anchored.relay(relay_port).await.expect("릴레이");
    overlay(&env, &image, "#!/bin/busybox sh\necho probe-started\n");

    let mut vm = sandbox
        .spawn(model_spec(&env, "no-rules"))
        .await
        .expect("VM");
    let out = lines(&mut vm).await;
    let exit = vm.wait().await;
    assert!(
        !out.iter().any(|l| l == "probe-started"),
        "init이 실패했는데 모델이 시작됐다: {out:?}"
    );
    assert_ne!(exit.code, Some(0), "{out:?}");
    assert!(
        exit.stderr_tail.contains("vm_init_failed"),
        "stderr: {}",
        exit.stderr_tail
    );
    sandbox.shutdown().await;
}
