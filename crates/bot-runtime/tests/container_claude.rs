//! 진짜 모델 이미지의 진짜 `claude`를 어댑터(닻·방화벽 init)로 VM에서 돌려, 실제 프런트·PG·워커 조립 전체를 확인한다(모델 호출 0: 프런트의 상류는 가짜다).
//! `BOT_MODEL_IMAGE=<이름@sha256:…>`·`BOT_CONTAINER_CANARY`·`BOT_TEST_DATABASE_URL`이 있어야 돈다.
//! - 호스트 쪽: 프런트는 닻이 알려 준 게이트웨이 주소에 열린다. VM의 `claude`는 그 프런트로만 모델 응답과 도구 결과(MCP)를 받는다.
//! - VM 안쪽: 응답이 흘러오는 도중에 `container exec`로 들여다본다. 진짜 `claude` 프로세스는 비root(10001)·capability 전부 0·NoNewPrivs이고, 같은 VM 안에서
//!   릴레이(프런트)만 닿으며 같은 게이트웨이의 다른 호스트 포트와 바깥 TCP는 버려진다.
//!
//! 실행: `BOT_CONTAINER_CANARY=required BOT_MODEL_IMAGE=… BOT_TEST_DATABASE_URL=… cargo test -p bot-runtime --test container_claude -- --nocapture`

mod support;
mod vm;

use bot_broker::testing::{End, FakeUpstream};
use bot_core::sandbox::ModelSandbox;
use bot_runtime::{Outcome, model::container::ContainerSandbox};
use std::{
    process::Command,
    sync::{Arc, Mutex},
    time::Duration,
};
use support::{cli::*, view::*, *};

/// VM 안에서 도는 `claude` 프로세스의 권한과, 지금 이 VM에서 닿는 곳(막힌 것은 시간 제한까지 기다리므로 나란히 점검한다).
const INSIDE: &str = r#"B=/bin/busybox
for p in /proc/[0-9]*; do
  case "$($B cat "$p/comm" 2>/dev/null)" in
    claude) $B grep -E '^(Uid|Gid|Groups|CapInh|CapPrm|CapEff|CapBnd|CapAmb|NoNewPrivs):' "$p/status" | $B tr -s '\t ' ' ' | $B sed 's/: */=/; s/ $//; s/^/claude./' ;;
  esac
done
try() {
  s=$($B date +%s)
  if echo "probe-$1" | $B nc -w 3 "$2" "$3" >/dev/null 2>&1; then echo "$1=connected"
  else
    d=$(( $($B date +%s) - s ))
    if [ "$d" -ge 2 ]; then echo "$1=dropped"; else echo "$1=refused"; fi
  fi
}
try relay @GATEWAY@ @RELAY@ >/tmp/p1 &
try other-port @GATEWAY@ @OTHER@ >/tmp/p2 &
try external-443 1.1.1.1 443 >/tmp/p3 &
try external-dns 8.8.8.8 53 >/tmp/p4 &
try wildcard @GATEWAY@ @WILD@ >/tmp/p5 &
wait
$B cat /tmp/p1 /tmp/p2 /tmp/p3 /tmp/p4 /tmp/p5
echo inspected
"#;

#[tokio::test(flavor = "multi_thread")]
async fn the_real_claude_in_the_vm_reaches_only_the_front_and_runs_unprivileged() {
    let Some(w) = world().await else {
        return;
    };
    let Some(program) = vm::available() else {
        return;
    };
    let _serial = vm::serial().await;
    let Some(image) = vm::model_image(&program) else {
        return;
    };
    let env = vm::Env::new(program.clone(), "claude");
    let mut config = env.firewalled(&image);
    config.cpus = 2;
    config.memory_mib = 2048;
    let anchored = ContainerSandbox::start(config).await.expect("시작");
    let gateway = anchored.address();
    // 같은 게이트웨이의 다른 호스트 서비스: VM은 여기에 닿으면 안 된다
    let (other_port, other_rx, _other) = vm::service(anchored.bind_addr(0));
    // 모든 인터페이스(0.0.0.0)에 연 호스트 서비스도 게이트웨이 주소로 닿는 길이다: 막혀야 한다
    let (wild_port, wild_rx, _wild) = vm::service("0.0.0.0:0".parse().unwrap());

    // 첫 응답은 읽기 도구(agenda.list)를 MCP로 부르고, 둘째 응답은 흘러오는 도중에 멈춰 있어(조각마다 3초) 그 사이 VM 안을 들여다본다
    let upstream = Arc::new(FakeUpstream::new());
    script(
        &upstream,
        tool_turn("msg_vm1", "toolu_vm1", "mcp__relay__agenda_list", "{}"),
    );
    let last = final_turn("msg_vm2", "VM 안에서 잘 돌아요");
    let refs: Vec<&[u8]> = last.iter().map(Vec::as_slice).collect();
    upstream.respond_slowly(200, &refs, Duration::from_secs(3), End::Eof);
    let run = w.accept("req-vm", "일정을 알려줘").await;

    let sandbox_slot = Mutex::new(None);
    let host = host_at(
        &w,
        w.pg.peer("worker-vm").await,
        upstream.clone(),
        anchored.bind_addr(0),
        async |front| {
            // 프런트가 실제로 연 주소가 릴레이다(어댑터가 받고 있는지 확인한다)
            let sandbox = Arc::new(anchored.relay(front.port()).await.expect("릴레이"));
            *sandbox_slot.lock().unwrap() = Some(sandbox.clone());
            sandbox as Arc<ModelSandbox>
        },
        &env.root.join("ws"),
        w.broker.clone(),
    )
    .await;
    let relay_port = host.front.addr().port();
    assert_eq!(host.front.addr().ip(), std::net::IpAddr::from(gateway));
    let sandbox = sandbox_slot.lock().unwrap().take().expect("샌드박스");

    let inside = INSIDE
        .replace("@GATEWAY@", &gateway.to_string())
        .replace("@RELAY@", &relay_port.to_string())
        .replace("@OTHER@", &other_port.to_string())
        .replace("@WILD@", &wild_port.to_string());
    let inspect = async {
        // 모델 VM(닻이 아닌 것)이 뜨고, 둘째 요청이 프런트를 거쳐 상류에 닿아(= claude가 도구 결과를 받았다) 응답을 기다리는 동안 본다
        let anchor = vm::running_anchor(&program, &env.instance).await;
        let mut vm_id = None;
        for _ in 0..1200 {
            vm_id = vm::ls_ids(&program, &format!("{}-", env.instance))
                .into_iter()
                .find(|id| *id != anchor);
            if vm_id.is_some() && upstream.requests().len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let vm_id = vm_id.expect("모델 VM이 떠야 한다");
        assert!(upstream.requests().len() >= 2, "둘째 요청이 와야 한다");
        let (program, inside) = (program.clone(), inside.clone());
        tokio::task::spawn_blocking(move || {
            let out = Command::new(&program)
                .args(["exec", &vm_id, "/bin/busybox", "sh", "-c", &inside])
                .output()
                .expect("container exec");
            String::from_utf8_lossy(&out.stdout).into_owned()
        })
        .await
        .unwrap()
    };
    let (outcome, report) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(150), host.worker.run_once()),
        inspect
    );
    eprintln!("{report}");
    let outcome = outcome.expect("실행이 끝나야 한다");
    assert_eq!(outcome, Some(Outcome::Succeeded));
    w.pg.assert_state(&run, "succeeded").await;
    assert_eq!(
        w.pg.scalar::<String>(&format!(
            "select result from bot_runs where id = '{}'",
            run.0
        ))
        .await,
        r#"{"text":"VM 안에서 잘 돌아요"}"#
    );

    // 진짜 `claude` 프로세스: 비root·보조 그룹 없음·capability 전부 0·NoNewPrivs. 릴레이만 닿는다
    let expect = |line: &str| {
        assert!(
            report.lines().any(|l| l == line),
            "{line}\n--- exec 출력\n{report}"
        )
    };
    expect("claude.Uid=10001 10001 10001 10001");
    expect("claude.Gid=10001 10001 10001 10001");
    for set in ["CapInh", "CapPrm", "CapEff", "CapBnd", "CapAmb"] {
        expect(&format!("claude.{set}=0000000000000000"));
    }
    expect("claude.NoNewPrivs=1");
    expect("relay=connected");
    expect("other-port=dropped");
    expect("wildcard=dropped");
    assert!(
        !report.contains("external-443=connected") && !report.contains("external-dns=connected"),
        "{report}"
    );
    expect("inspected");
    assert!(
        other_rx.try_recv().is_err(),
        "다른 호스트 포트가 받은 것이 있다"
    );
    assert!(
        wild_rx.try_recv().is_err(),
        "0.0.0.0 서비스가 받은 것이 있다"
    );

    // 상류(프런트의 뒤)가 받은 요청은 정확히 둘이고, 둘째에는 MCP로 실행한 도구의 실제 결과가 있다. 세션 줄은 체크포인트로 남았다
    let seen = bodies(&upstream);
    assert_eq!(seen.len(), 2);
    assert!(
        result_text(&seen[1], "toolu_vm1").is_some(),
        "도구 결과가 둘째 요청에 있다"
    );
    assert!(
        w.runner.jobs().len() == 1,
        "도구는 한 번 실행됐다: {:?}",
        w.runner.jobs()
    );
    let keys = step_keys(&w, &run).await;
    assert!(keys.iter().any(|k| k.starts_with("cli-rec-")), "{keys:?}");

    // 끝난 모델 VM은 사라지고 닻만 남으며, 종료는 닻도 치운다
    env.until(20, "닻만 남아야 한다", |e| e.ids().len() == 1)
        .await;
    sandbox.shutdown().await;
    assert!(env.ids().is_empty(), "{:?}", env.ids());
}
