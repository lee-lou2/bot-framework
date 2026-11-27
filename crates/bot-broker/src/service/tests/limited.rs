//! Rate and concurrency limiter tests verifying `Error::RateLimited` stays distinct from `Error::Unavailable`.

use super::{
    schedule::{OWNER, WITH_SCHEDULE, snapshot_with},
    secret_check::needy,
    *,
};

#[test]
fn only_a_refusal_by_the_limiter_returns_rate_limited() {
    let w = World::with(cfg_with_rate(60, 1, 4));
    assert!(w.call("cli-4-0", "agenda.list", "{}").is_ok());
    assert!(matches!(
        w.call("cli-4-1", "agenda.list", "{}"),
        Err(Error::RateLimited)
    ));
    w.clock.advance(Duration::from_secs(60));
    w.ledger.set_context_down(true);
    assert!(matches!(
        w.call("cli-4-1", "agenda.list", "{}"),
        Err(Error::Unavailable)
    ));
    w.ledger.set_context_down(false);
    done(w.call("cli-4-2", "agenda.list", "{}"));
    assert!(matches!(
        w.call("cli-4-3", "agenda.list", "{}"),
        Err(Error::RateLimited)
    ));
}

#[test]
fn rate_limited_covers_the_limit_in_front_of_the_secret_check_and_the_builtin_tools_too() {
    let w = World::with(cfg_with_rate(60, 1, 4));
    w.publish(
        base("r1")
            .secret("mail-token")
            .tool("main", "agenda.mail", needy(Risk::Read))
            .build(),
    );
    w.runner.miss_secret("mail-token");
    assert!(matches!(
        w.call("cli-5-0", "agenda.mail", "{}"),
        Err(Error::Denied)
    ));
    assert!(matches!(
        w.call("cli-5-1", "agenda.mail", "{}"),
        Err(Error::RateLimited)
    ));
    assert!(!w.ledger.has_call("r1", "cli-5-1"));
    let w = World::with(cfg_with_rate(60, 1, 4));
    w.publish(snapshot_with("r1").build());
    w.ledger.add_run(run_context(
        "personal",
        "r1",
        1,
        OWNER,
        "main",
        &WITH_SCHEDULE,
    ));
    assert!(w.call("cli-6-0", "schedule.list", "{}").is_ok());
    assert!(matches!(
        w.call("cli-6-1", "schedule.list", "{}"),
        Err(Error::RateLimited)
    ));
}

#[test]
fn a_concurrency_limit_refusal_returns_rate_limited() {
    let w = World::with(cfg_with_rate(600, 20, 1));
    let gate = w.runner.hold_next();
    std::thread::scope(|scope| {
        let running = scope.spawn(|| w.call("cli-7-0", "agenda.list", "{}"));
        let started = std::time::Instant::now();
        while w.runner.jobs().is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "in-flight call did not start"
            );
            std::thread::yield_now();
        }
        assert!(matches!(
            w.call("cli-7-1", "agenda.list", "{}"),
            Err(Error::RateLimited)
        ));
        gate.add_permits(1);
        let ran = running.join().expect("in-flight call");
        assert!(ran.is_ok(), "in-flight call must not be rate-limited");
    });
    done(w.call("cli-7-1", "agenda.list", "{}"));
}

#[test]
fn two_calls_in_flight_together_never_confuse_their_error_kinds() {
    let w = World::with(cfg_with_rate(60, 1, 4));
    done(w.call("cli-8-0", "agenda.list", "{}"));
    let (limited_call, other_call) = block_on(async {
        tokio::join!(
            w.broker
                .call(w.tool_call("r1", 1, "cli-8-1", "agenda.list", "{}")),
            w.broker
                .call(w.tool_call("r1", 1, "cli-8-2", "agenda.absent", "{}")),
        )
    });
    assert!(matches!(limited_call, Err(Error::RateLimited)));
    assert!(
        matches!(other_call, Err(Error::Missing)),
        "concurrent call rejected before limiter got wrong error: {other_call:?}"
    );
}
