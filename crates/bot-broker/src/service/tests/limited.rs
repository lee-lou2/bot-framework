//! 속도·동시성 한도가 **이 호출을 거절했는지**는 그 호출의 결과와 함께 온다(`noting_limit`). 오류는 늘 `Unavailable`이라 저장소 장애 같은 다른 이유의
//! `Unavailable`과 구별하려는 원인이다: 한도가 거절한 자리(자격 확인 앞·일반 호출 흐름·내장 도구)에서만 켜지고, 표지가 호출마다 따로라 다른 호출이나 읽히지
//! 않은 이전 거절과 섞이지 않는다.

use super::{
    schedule::{OWNER, WITH_SCHEDULE, snapshot_with},
    secret_check::needy,
    *,
};
use crate::service::preflight::noting_limit;

/// 호출 한 번과 «한도가 이 호출을 거절했는가»를 함께 받는다.
fn noted(w: &World, call: &str, tool: &str) -> (Result<CallResult, Error>, bool) {
    w.call_noted(call, tool, "{}")
}

#[test]
fn only_a_refusal_by_the_limiter_is_noted_with_that_very_call() {
    let w = World::with(cfg_with_rate(60, 1, 4));
    let (ran, limited) = noted(&w, "cli-4-0", "agenda.list");
    assert!(ran.is_ok() && !limited, "통과한 호출은 한도 거절이 아니다");
    // 한도가 거절했다
    let (refused, limited) = noted(&w, "cli-4-1", "agenda.list");
    assert!(matches!(refused, Err(Error::Unavailable)) && limited);
    // 표지를 읽지 않아도 아무것도 남지 않는다: 같은 위치를 다시 불렀을 때 이번 호출이 한도에 걸린 것이 아니면 한도 거절이 아니다
    // (한도가 회복되고 저장소가 막힌 경우: 저장소 장애의 `Unavailable`은 한도가 아니다)
    w.clock.advance(Duration::from_secs(60));
    w.ledger.set_context_down(true);
    let (outage, limited) = noted(&w, "cli-4-1", "agenda.list");
    assert!(matches!(outage, Err(Error::Unavailable)));
    assert!(!limited, "저장소 장애는 한도 거절이 아니다");
    // 감싸지 않은 호출도 같은 오류로 거절된다(표지가 없을 뿐이다)
    w.ledger.set_context_down(false);
    done(w.call("cli-4-2", "agenda.list", "{}"));
    assert!(matches!(
        w.call("cli-4-3", "agenda.list", "{}"),
        Err(Error::Unavailable)
    ));
}

#[test]
fn the_note_covers_the_limit_in_front_of_the_secret_check_and_the_builtin_tools_too() {
    // 자격 확인 앞의 한도: 호출 기록도 보관소 조회도 없이 돌려보낸 자리다
    let w = World::with(cfg_with_rate(60, 1, 4));
    w.publish(
        base("r1")
            .secret("mail-token")
            .tool("main", "agenda.mail", needy(Risk::Read))
            .build(),
    );
    w.runner.miss_secret("mail-token");
    let (denied, limited) = noted(&w, "cli-5-0", "agenda.mail");
    assert!(matches!(denied, Err(Error::Denied)));
    assert!(!limited, "자격 거절은 한도가 아니다");
    let (refused, limited) = noted(&w, "cli-5-1", "agenda.mail");
    assert!(matches!(refused, Err(Error::Unavailable)));
    assert!(!w.ledger.has_call("r1", "cli-5-1"));
    assert!(limited);
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
    let (ran, limited) = noted(&w, "cli-6-0", "schedule.list");
    assert!(ran.is_ok() && !limited);
    let (refused, limited) = noted(&w, "cli-6-1", "schedule.list");
    assert!(matches!(refused, Err(Error::Unavailable)) && limited);
}

#[test]
fn a_concurrency_limit_refusal_is_noted_like_a_rate_refusal() {
    let w = World::with(cfg_with_rate(600, 20, 1));
    let gate = w.runner.hold_next();
    std::thread::scope(|scope| {
        // 자리 하나를 쥔 채 멈춘 호출이 있는 동안 다른 호출은 동시성 한도에 걸린다
        let running = scope.spawn(|| noted(&w, "cli-7-0", "agenda.list"));
        let started = std::time::Instant::now();
        while w.runner.jobs().is_empty() {
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "실행이 시작되지 않는다"
            );
            std::thread::yield_now();
        }
        let (refused, limited) = noted(&w, "cli-7-1", "agenda.list");
        assert!(matches!(refused, Err(Error::Unavailable)) && limited);
        gate.add_permits(1);
        let (ran, limited) = running.join().expect("실행하던 호출");
        assert!(
            ran.is_ok() && !limited,
            "실행하던 호출은 한도에 걸리지 않았다"
        );
    });
    // 자리가 돌아오면 같은 위치를 다시 처리한다
    done(w.call("cli-7-1", "agenda.list", "{}"));
}

#[test]
fn two_calls_in_flight_together_never_share_a_note() {
    let w = World::with(cfg_with_rate(60, 1, 4));
    done(w.call("cli-8-0", "agenda.list", "{}"));
    // 한 호출은 한도에 걸리고 다른 호출은 한도 앞에서 다른 이유로 거절된다(같은 스레드에서 번갈아 폴링된다)
    let ((limited_call, limited), (other_call, other)) = block_on(async {
        tokio::join!(
            noting_limit(
                w.broker
                    .call(w.tool_call("r1", 1, "cli-8-1", "agenda.list", "{}"))
            ),
            noting_limit(
                w.broker
                    .call(w.tool_call("r1", 1, "cli-8-2", "agenda.absent", "{}"))
            ),
        )
    });
    assert!(matches!(limited_call, Err(Error::Unavailable)) && limited);
    assert!(
        other_call.is_err() && !other,
        "한도 앞에서 거절된 호출에 다른 호출의 표지가 섞였다: {other_call:?}"
    );
}
