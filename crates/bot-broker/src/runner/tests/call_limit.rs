//! 도구별 호출 시간 한도(`timeout_secs`)와 호스트 천장: 선언한 한도의 적용, 선언하지 않은 도구의 기본 한도 유지, 천장을 넘으면 보내기 전에 거절, wall·터널 계산,
//! 원격 서버도 같은 값. 시간 초과는 보낸 뒤라면 늘 `Unknown`(자동 재송신 없음)이다.
use super::*;
use crate::{
    mcp::remote::Remote,
    testing::{Era, FakeMcp, FakeWire, McpTool},
};
use std::time::Instant;

const EVENT: &str = r#"{"event":"a"}"#;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn declared(secs: Option<u32>) -> Tool {
    let mut tool = decl(&[], &[], false);
    tool.timeout_secs = secs;
    tool
}

fn hanging() -> Server {
    Server::new(&[("delete", schema())]).replying(|| Answer::Hang)
}

#[tokio::test]
async fn a_declared_limit_replaces_the_default_for_that_tool_only_and_a_timeout_is_never_resent() {
    // rig의 기본 한도는 300ms다(선언하지 않은 도구). 선언하지 않은 도구는 거기서 끊긴다.
    let r = rig(hanging(), |_| {}, |_| {});
    let started = Instant::now();
    assert!(matches!(
        run(&r, job(declared(None), EVENT)).await,
        Ran::Unknown("timeout")
    ));
    assert!(started.elapsed() < ms(900), "{:?}", started.elapsed());
    // 1초를 선언한 도구는 기본 한도에 끊기지 않고 1초를 기다린다. 보낸 호출은 한 번뿐이고 프로세스는 끝낸다.
    let r = rig(hanging(), |_| {}, |_| {});
    let started = Instant::now();
    assert!(matches!(
        run(&r, job(declared(Some(1)), EVENT)).await,
        Ran::Unknown("timeout")
    ));
    assert!(started.elapsed() >= ms(900), "{:?}", started.elapsed());
    assert_eq!(r.sandbox.raw_calls().len(), 1, "다시 보내지 않는다");
    assert_eq!(r.sandbox.terminated().len(), 1);
}

#[tokio::test]
async fn a_limit_above_the_host_ceiling_is_refused_before_anything_starts() {
    let tool = |secs| {
        let mut tool = decl(&[("TOKEN", "reader")], &[], false);
        tool.timeout_secs = secs;
        tool
    };
    // 천장 2초인데 3초를 선언했다(활성화 뒤에 천장을 내렸거나 복원한 옛 선언)
    let r = rig(
        hanging(),
        |c| c.max_call_timeout = Duration::from_secs(2),
        |_| {},
    );
    let ran = run(&r, job(tool(Some(3)), EVENT)).await;
    assert!(
        matches!(ran, Ran::Refused("tool_timeout_over_cap")),
        "{ran:?}"
    );
    assert!(r.sandbox.specs().is_empty(), "프로세스를 띄우지 않았다");
    assert!(r.secrets.resolved().is_empty(), "자격을 풀지 않았다");
    assert!(r.sandbox.raw_calls().is_empty(), "아무것도 보내지 않았다");
    // 선언하지 않아도 기본 한도(여기서는 300ms)가 천장(100ms)을 넘으면 거절한다
    let r = rig(hanging(), |c| c.max_call_timeout = ms(100), |_| {});
    let ran = run(&r, job(tool(None), EVENT)).await;
    assert!(
        matches!(ran, Ran::Refused("tool_timeout_over_cap")),
        "{ran:?}"
    );
    assert!(r.sandbox.specs().is_empty());
    // 천장과 같으면 보낸다
    let r = rig(
        Server::new(&[("delete", schema())]),
        |c| c.max_call_timeout = Duration::from_secs(3),
        |_| {},
    );
    let ran = run(&r, job(tool(Some(3)), EVENT)).await;
    assert!(matches!(ran, Ran::Done(_)), "{ran:?}");
}

#[tokio::test]
async fn the_wall_stretches_only_for_a_longer_declared_limit_so_the_call_limit_decides_first() {
    // 기본 한도 100ms·wall 400ms. 1초를 선언하면 wall = 400ms + (1s − 100ms) = 1.3초이므로 호출 한도(1초)가 먼저 끝낸다(`wall_timeout`이 아니라 `timeout`).
    let small = |c: &mut RunnerConfig| {
        c.call_timeout = ms(100);
        c.wall = ms(400);
    };
    let r = rig(hanging(), small, |_| {});
    let ran = run(&r, job(declared(Some(1)), EVENT)).await;
    assert!(matches!(ran, Ran::Unknown("timeout")), "{ran:?}");
    assert_eq!(r.sandbox.specs()[0].limits.wall, ms(1300));
    // 선언하지 않은 도구의 wall은 그대로다
    let r = rig(hanging(), small, |_| {});
    let ran = run(&r, job(declared(None), EVENT)).await;
    assert!(matches!(ran, Ran::Unknown("timeout")), "{ran:?}");
    assert_eq!(r.sandbox.specs()[0].limits.wall, ms(400));
    // 기본보다 짧게 선언하면 호출만 줄고 wall은 줄지 않는다
    let r = rig(
        hanging(),
        |c| {
            c.call_timeout = Duration::from_secs(5);
            c.wall = Duration::from_secs(30);
        },
        |_| {},
    );
    let started = Instant::now();
    let ran = run(&r, job(declared(Some(1)), EVENT)).await;
    assert!(matches!(ran, Ran::Unknown("timeout")), "{ran:?}");
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(r.sandbox.specs()[0].limits.wall, Duration::from_secs(30));
}

fn weather() -> Remote {
    Remote::parse("https://mcp.example/mcp", None).unwrap()
}

/// 원격 서버 `weather`로 가는 호출(샌드박스 없음). 서버가 내세우는 도구는 `delete` 하나다.
fn remote_rig(server: Arc<FakeMcp>, cfg: impl FnOnce(&mut RunnerConfig)) -> (Rig, Arc<FakeWire>) {
    let target = weather();
    let mut base = rig(Server::new(&[]), cfg, |host| {
        host.servers.insert("weather".into(), target.revision());
        host.remote.insert("weather".into(), target);
    });
    let wire = Arc::new(FakeWire::new(server));
    base.runner = base.runner.with_wire(wire.clone());
    (base, wire)
}

fn remote_tool(secs: Option<u32>) -> Tool {
    let mut tool = decl(&[], &["mcp.example:443"], false);
    tool.server = "weather".into();
    tool.revision = Revision(format!("t1@{}", weather().revision()));
    tool.timeout_secs = secs;
    tool
}

fn delete_def() -> McpTool {
    McpTool::new("delete", "일정을 지운다", schema())
}

#[tokio::test]
async fn a_remote_server_gets_the_same_effective_limit_and_the_wire_carries_it() {
    // 선언한 한도(1초)가 기본 한도(300ms)를 대신한다: 응답이 끝나지 않는 서버에서 1초 뒤 `timeout`이고 다시 보내지 않는다
    let mut fake = FakeMcp::new(Era::Modern, vec![delete_def()]);
    fake.hang = Some("tools/call".into());
    let (r, wire) = remote_rig(Arc::new(fake), |_| {});
    let started = Instant::now();
    let ran = run(&r, job(remote_tool(Some(1)), EVENT)).await;
    assert!(matches!(ran, Ran::Unknown("timeout")), "{ran:?}");
    assert!(started.elapsed() >= ms(900), "{:?}", started.elapsed());
    // `tools/call`만 호출 한도 + 여유의 전송 제한을 싣는다
    let seen = wire.timeouts.lock().unwrap().clone();
    let call: Vec<_> = seen.iter().filter(|(rpc, _)| rpc == "tools/call").collect();
    assert_eq!(call.len(), 1, "다시 보내지 않는다: {seen:?}");
    assert_eq!(
        call[0].1,
        Some(Duration::from_secs(1) + Duration::from_secs(5))
    );
    assert!(
        seen.iter()
            .filter(|(rpc, _)| rpc != "tools/call")
            .all(|(_, limit)| limit.is_none())
    );
    // 선언하지 않은 도구는 기본 한도(300ms)에서 끊긴다
    let mut fake = FakeMcp::new(Era::Modern, vec![delete_def()]);
    fake.hang = Some("tools/call".into());
    let (r, wire) = remote_rig(Arc::new(fake), |_| {});
    let started = Instant::now();
    let ran = run(&r, job(remote_tool(None), EVENT)).await;
    assert!(matches!(ran, Ran::Unknown("timeout")), "{ran:?}");
    assert!(started.elapsed() < ms(900), "{:?}", started.elapsed());
    let seen = wire.timeouts.lock().unwrap().clone();
    assert!(seen.iter().any(
        |(rpc, limit)| rpc == "tools/call" && *limit == Some(ms(300) + Duration::from_secs(5))
    ));
}

#[tokio::test]
async fn a_remote_limit_above_the_host_ceiling_sends_nothing() {
    let fake = Arc::new(FakeMcp::new(Era::Modern, vec![delete_def()]));
    let (r, wire) = remote_rig(Arc::clone(&fake), |c| {
        c.max_call_timeout = Duration::from_secs(2)
    });
    let ran = run(&r, job(remote_tool(Some(3)), EVENT)).await;
    assert!(
        matches!(ran, Ran::Refused("tool_timeout_over_cap")),
        "{ran:?}"
    );
    assert!(
        wire.targets.lock().unwrap().is_empty(),
        "연결도 하지 않았다"
    );
    assert_eq!(fake.count("tools/call"), 0);
}
