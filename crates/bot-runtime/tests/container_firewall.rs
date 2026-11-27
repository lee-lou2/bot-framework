//! Canary test for the VM firewall boundary using the real model image's `bot-init` entrypoint.
//! Overlays `/usr/local/bin/claude` with a probe script while keeping `bot-init` intact to verify egress is restricted to the relay IP:port, tampering is denied, and init failure aborts before execing the model.
//! Run: `BOT_CONTAINER_CANARY=required BOT_MODEL_IMAGE=... cargo test -p bot-runtime --test container_firewall -- --nocapture`

mod vm;

use std::{
    net::{SocketAddr, TcpStream},
    os::unix::fs::PermissionsExt,
    process::Command,
    time::Duration,
};
use vm::*;

const NEIGHBOR_PORT: u16 = 7000;

/// Probe script executed in place of `claude` after `bot-init` drops privileges; runs blocked probes in parallel to distinguish timeout (`dropped`) from immediate RST (`refused`).
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

/// Copies `bot-init` out of the image alongside the probe script as `claude` for bind-mounting over `/usr/local/bin`.
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
            // Run on the test network so vmnet does not briefly drop the host-only gateway address.
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
    assert!(copied, "failed to extract bot-init from model image");
    std::fs::write(bin.join("claude"), probe).unwrap();
    for name in ["bot-init", "claude"] {
        std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

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
        .expect("start");
    let gateway = anchored.address().to_string();
    let (relay_port, relay_rx, _relay) = service(anchored.bind_addr(0));
    let (other_port, other_rx, _other) = service(anchored.bind_addr(0));
    // Services bound on 0.0.0.0 are also reachable via the gateway IP without the VM firewall.
    let (wild_port, wild_rx, _wild) = service("0.0.0.0:0".parse().unwrap());
    let sandbox = anchored.relay(relay_port).await.expect("relay");
    while relay_rx.try_recv().is_ok() {}

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
    let neighbor_ip = neighbor_ip.expect("neighbor VM must be reachable from host (control)");
    let anchor_ip = ip_of(&program, &running_anchor(&program, &env.instance).await).unwrap();
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
        "anchor must refuse closed port from host (control)"
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

    // Control without `bot-init`: proves drops below come from nft rules rather than network setup.
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
            .expect("control VM");
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
        assert!(open.lines().any(|l| l == line), "control: {line}\n{open}");
    }
    let reached: Vec<String> =
        std::iter::from_fn(|| other_rx.recv_timeout(Duration::from_millis(800)).ok()).collect();
    assert!(
        reached.iter().any(|m| m == "probe-other-port"),
        "control VM must reach other host port: {reached:?}"
    );
    while relay_rx.try_recv().is_ok() {}
    let wild_reached: Vec<String> =
        std::iter::from_fn(|| wild_rx.recv_timeout(Duration::from_millis(800)).ok()).collect();
    assert!(
        wild_reached.iter().any(|m| m == "probe-wildcard"),
        "control VM must reach 0.0.0.0 service: {wild_reached:?}"
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
    expect("ids=10001:10001 groups=10001");
    for set in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        expect(&format!("{set}=0000000000000000"));
    }
    expect("NoNewPrivs=1");
    expect(&format!("url=http://{gateway}:{relay_port}"));
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
    for n in 1..=6 {
        expect(&format!("tamper-{n}=denied"));
    }
    expect("control-ip=ok");
    expect("work=written");
    expect("done");
    assert_eq!(exit.code, Some(0), "{text}");
    let written = env.root.join("work/probe.txt");
    assert_eq!(
        std::fs::read_to_string(&written).unwrap().trim(),
        "from-10001"
    );
    std::fs::remove_file(&written).expect("host must be able to remove guest-written file");
    let got: Vec<String> =
        std::iter::from_fn(|| relay_rx.recv_timeout(Duration::from_secs(2)).ok()).collect();
    assert!(got.iter().all(|m| m.starts_with("probe-relay")), "{got:?}");
    assert!(!got.is_empty(), "relay received nothing");
    assert!(
        other_rx.try_recv().is_err(),
        "other host port received data"
    );
    assert!(
        wild_rx.recv_timeout(Duration::from_millis(500)).is_err(),
        "firewalled VM reached 0.0.0.0 service"
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
    // Omit NET_ADMIN so `bot-init` fails to apply nft rules and exits before execing the model.
    config.entry = Some(Entry {
        model_caps: ["SETGID", "SETPCAP", "SETUID"].map(String::from).into(),
        ..Entry::model_image()
    });
    let anchored = ContainerSandbox::start(config).await.expect("start");
    let (relay_port, _rx, _relay) = service(anchored.bind_addr(0));
    let sandbox = anchored.relay(relay_port).await.expect("relay");
    overlay(&env, &image, "#!/bin/busybox sh\necho probe-started\n");

    let mut vm = sandbox
        .spawn(model_spec(&env, "no-rules"))
        .await
        .expect("VM");
    let out = lines(&mut vm).await;
    let exit = vm.wait().await;
    assert!(
        !out.iter().any(|l| l == "probe-started"),
        "model started despite init failure: {out:?}"
    );
    assert_ne!(exit.code, Some(0), "{out:?}");
    assert!(
        exit.stderr_tail.contains("vm_init_failed"),
        "stderr: {}",
        exit.stderr_tail
    );
    sandbox.shutdown().await;
}
