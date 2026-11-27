//! Per-tool `timeout_secs` enforcement, host ceiling rejection, wall adjustment, and remote parity.
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
    // Default rig call timeout is 300ms.
    let r = rig(hanging(), |_| {}, |_| {});
    let started = Instant::now();
    assert!(matches!(
        run(&r, job(declared(None), EVENT)).await,
        Ran::Unknown("timeout")
    ));
    assert!(started.elapsed() < ms(900), "{:?}", started.elapsed());
    // A 1s declared limit overrides the 300ms default without retrying on timeout.
    let r = rig(hanging(), |_| {}, |_| {});
    let started = Instant::now();
    assert!(matches!(
        run(&r, job(declared(Some(1)), EVENT)).await,
        Ran::Unknown("timeout")
    ));
    assert!(started.elapsed() >= ms(900), "{:?}", started.elapsed());
    assert_eq!(r.sandbox.raw_calls().len(), 1, "must not retry");
    assert_eq!(r.sandbox.terminated().len(), 1);
}

#[tokio::test]
async fn a_limit_above_the_host_ceiling_is_refused_before_anything_starts() {
    let tool = |secs| {
        let mut tool = decl(&[("TOKEN", "reader")], &[], false);
        tool.timeout_secs = secs;
        tool
    };
    // Declared 3s exceeds the 2s host ceiling.
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
    assert!(r.sandbox.specs().is_empty(), "must not spawn process");
    assert!(r.secrets.resolved().is_empty(), "must not resolve secrets");
    assert!(r.sandbox.raw_calls().is_empty(), "must not send call");
    // Default timeout (300ms) exceeding ceiling (100ms) is also refused.
    let r = rig(hanging(), |c| c.max_call_timeout = ms(100), |_| {});
    let ran = run(&r, job(tool(None), EVENT)).await;
    assert!(
        matches!(ran, Ran::Refused("tool_timeout_over_cap")),
        "{ran:?}"
    );
    assert!(r.sandbox.specs().is_empty());
    // Equal to ceiling is allowed.
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
    // Base 100ms + wall 400ms stretches to 1.3s for a 1s call limit so call timeout fires before wall.
    let small = |c: &mut RunnerConfig| {
        c.call_timeout = ms(100);
        c.wall = ms(400);
    };
    let r = rig(hanging(), small, |_| {});
    let ran = run(&r, job(declared(Some(1)), EVENT)).await;
    assert!(matches!(ran, Ran::Unknown("timeout")), "{ran:?}");
    assert_eq!(r.sandbox.specs()[0].limits.wall, ms(1300));
    let r = rig(hanging(), small, |_| {});
    let ran = run(&r, job(declared(None), EVENT)).await;
    assert!(matches!(ran, Ran::Unknown("timeout")), "{ran:?}");
    assert_eq!(r.sandbox.specs()[0].limits.wall, ms(400));
    // Shorter declared limit narrows call timeout without shrinking wall.
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

/// Remote rig for `weather` serving a single `delete` tool without a sandbox.
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
    McpTool::new("delete", "Delete an event", schema())
}

#[tokio::test]
async fn a_remote_server_gets_the_same_effective_limit_and_the_wire_carries_it() {
    // Declared 1s overrides the 300ms default and times out once without retry.
    let mut fake = FakeMcp::new(Era::Modern, vec![delete_def()]);
    fake.hang = Some("tools/call".into());
    let (r, wire) = remote_rig(Arc::new(fake), |_| {});
    let started = Instant::now();
    let ran = run(&r, job(remote_tool(Some(1)), EVENT)).await;
    assert!(matches!(ran, Ran::Unknown("timeout")), "{ran:?}");
    assert!(started.elapsed() >= ms(900), "{:?}", started.elapsed());
    // Only `tools/call` carries the call timeout plus transport slack.
    let seen = wire.timeouts.lock().unwrap().clone();
    let call: Vec<_> = seen.iter().filter(|(rpc, _)| rpc == "tools/call").collect();
    assert_eq!(call.len(), 1, "must not retry: {seen:?}");
    assert_eq!(
        call[0].1,
        Some(Duration::from_secs(1) + Duration::from_secs(5))
    );
    assert!(
        seen.iter()
            .filter(|(rpc, _)| rpc != "tools/call")
            .all(|(_, limit)| limit.is_none())
    );
    // Undeclared tool times out at the 300ms default.
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
    assert!(wire.targets.lock().unwrap().is_empty(), "must not connect");
    assert_eq!(fake.count("tools/call"), 0);
}
