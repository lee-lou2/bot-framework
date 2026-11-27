//! 속도·동시성 한도 거절은 브로커가 직접 `Error::RateLimited`를 돌려준다. 저장소 장애 같은 다른 이유의
//! `Error::Unavailable`과 타입 수준에서 구분되며, 한도가 거절한 자리(자격 확인 앞·일반 호출 흐름·내장 도구)에서만
//! 반환되어 다른 호출이나 이전 거절과 절대 섞이지 않는다.

use super::{
    schedule::{OWNER, WITH_SCHEDULE, snapshot_with},
    secret_check::needy,
    *,
};

#[test]
fn only_a_refusal_by_the_limiter_returns_rate_limited() {
    let w = World::with(cfg_with_rate(60, 1, 4));
    assert!(w.call("cli-4-0", "agenda.list", "{}").is_ok());
    // 한도가 거절했다
    assert!(matches!(
        w.call("cli-4-1", "agenda.list", "{}"),
        Err(Error::RateLimited)
    ));
    // 한도가 회복되고 저장소가 막힌 경우: 저장소 장애는 `Unavailable`이다(한도 거절과 구분된다)
    w.clock.advance(Duration::from_secs(60));
    w.ledger.set_context_down(true);
    assert!(matches!(
        w.call("cli-4-1", "agenda.list", "{}"),
        Err(Error::Unavailable)
    ));
    // 저장소가 돌아오면 정상 호출 뒤 다음 호출이 다시 `RateLimited`다
    w.ledger.set_context_down(false);
    done(w.call("cli-4-2", "agenda.list", "{}"));
    assert!(matches!(
        w.call("cli-4-3", "agenda.list", "{}"),
        Err(Error::RateLimited)
    ));
}

#[test]
fn rate_limited_covers_the_limit_in_front_of_the_secret_check_and_the_builtin_tools_too() {
    // 자격 확인 앞의 한도: 호출 기록도 보관소 조회도 없이 돌려보낸 자리다
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
    // 내장 예약 도구도 같다
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
        // 자리 하나를 쥔 채 멈춘 호출이 있는 동안 다른 호출은 동시성 한도에 걸린다
        let running = scope.spawn(|| w.call("cli-7-0", "agenda.list", "{}"));
        let started = std::time::Instant::now();
        while w.runner.jobs().is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "실행이 시작되지 않는다"
            );
            std::thread::yield_now();
        }
        assert!(matches!(
            w.call("cli-7-1", "agenda.list", "{}"),
            Err(Error::RateLimited)
        ));
        gate.add_permits(1);
        let ran = running.join().expect("실행하던 호출");
        assert!(ran.is_ok(), "실행하던 호출은 한도에 걸리지 않았다");
    });
    // 자리가 돌아오면 같은 위치를 다시 처리한다
    done(w.call("cli-7-1", "agenda.list", "{}"));
}

#[test]
fn two_calls_in_flight_together_never_confuse_their_error_kinds() {
    let w = World::with(cfg_with_rate(60, 1, 4));
    done(w.call("cli-8-0", "agenda.list", "{}"));
    // 한 호출은 한도에 걸리고 다른 호출은 한도 앞에서 다른 이유로 거절된다(같은 스레드에서 번갈아 폴링된다)
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
        "한도 앞에서 거절된 호출의 오류가 달라졌다: {other_call:?}"
    );
}
