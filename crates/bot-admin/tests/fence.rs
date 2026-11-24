//! «같은 집합 확인» 훅: 옛 활성화로 남은 프로세스가 조회 결과를 내주지 않는다.

mod support;
use bot_admin::{FenceFuture, fake::Fake};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};
use support::*;

/// 호출 수를 세고 `same`이 가리키는 값을 돌려주는 훅.
fn hook(
    same: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
) -> impl Fn() -> FenceFuture + Send + Sync + 'static {
    move || {
        calls.fetch_add(1, SeqCst);
        let v = same.load(SeqCst);
        Box::pin(async move { v })
    }
}

async fn fenced(same: bool) -> (Server, Arc<AtomicBool>, Arc<AtomicUsize>) {
    let (flag, calls) = (
        Arc::new(AtomicBool::new(same)),
        Arc::new(AtomicUsize::new(0)),
    );
    let (f, c) = (flag.clone(), calls.clone());
    let s = start_cfg(Fake::demo(), move |cfg| cfg.fence(hook(f, c))).await;
    (s, flag, calls)
}

const GETS: [&str; 9] = [
    "/v1/admin/me",
    "/v1/admin/runs",
    "/v1/admin/runs/personal/r-1",
    "/v1/admin/approvals",
    "/v1/admin/holds",
    "/v1/admin/audit",
    "/v1/admin/audit/verify",
    "/v1/admin/status",
    "/v1/admin/map",
];

#[tokio::test]
async fn a_matching_set_changes_nothing_and_is_checked_once_per_read_and_twice_per_write() {
    let (s, _, calls) = fenced(true).await;
    for path in GETS {
        let before = calls.load(SeqCst);
        let r = get(&s, OPERATOR, path).await;
        assert_eq!(r.status, 200, "{path}");
        assert_eq!(
            calls.load(SeqCst) - before,
            1,
            "{path}: 조회는 응답 직전에 한 번"
        );
    }
    let before = calls.load(SeqCst);
    let seen = bot_admin::json::hex(&bot_admin::fake::digest("c-1:agenda.delete"));
    let r = post(
        &s,
        OPERATOR,
        "/v1/admin/approvals/ap-1/decide",
        &format!(r#"{{"allow":true,"seen":"{seen}"}}"#),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // 변경 요청은 CSRF 토큰을 받는 조회(`/me`) 한 번과 판정의 «핸들러 전·응답 직전» 두 번이다.
    assert_eq!(calls.load(SeqCst) - before, 1 + 2);
    // 모르는 경로는 이전처럼 404다.
    assert_eq!(get(&s, OPERATOR, "/v1/admin/nothing").await.status, 404);
}

#[tokio::test]
async fn a_stale_set_closes_every_json_route_with_an_empty_unavailable_answer() {
    let (s, _, _) = fenced(false).await;
    let mut all: Vec<String> = GETS.iter().map(|p| (*p).to_owned()).collect();
    all.push("/v1/admin/nothing".to_owned());
    for path in all {
        let r = get(&s, OPERATOR, &path).await;
        assert_eq!(
            (r.status, r.body.as_str()),
            (503, r#"{"error":"unavailable"}"#),
            "{path}"
        );
        assert_eq!(r.header("cache-control"), Some("no-store"), "{path}");
        // 프로필 이름·신원·비밀·CSRF 토큰·내용이 하나도 없다.
        for secret in [
            "personal",
            "work",
            "local:owner",
            OPERATOR,
            "csrf",
            "r-1",
            "ap-1",
            "agenda",
        ] {
            assert!(!r.body.contains(secret), "{path}: {secret}");
        }
    }
    // 인증과 CSRF 확인이 훅보다 먼저다: 토큰이 틀린 변경 요청은 훅까지 가지 않고 403이다.
    let decide = raw(
        s.addr,
        "POST",
        "/v1/admin/approvals/ap-1/decide",
        None,
        &[
            ("Authorization", "Bearer operator-secret-1"),
            ("Content-Type", "application/json"),
            ("X-Bot-Csrf", "x"),
        ],
        Some("{}"),
    )
    .await;
    assert_eq!(decide.status, 403);
}

#[tokio::test]
async fn writes_never_start_when_the_set_is_stale_and_resume_cleanly_when_it_matches_again() {
    let (s, flag, _) = fenced(true).await;
    let csrf = csrf(&s, OPERATOR).await;
    let seen = bot_admin::json::hex(&bot_admin::fake::digest("c-1:agenda.delete"));
    let body = format!(r#"{{"allow":true,"seen":"{seen}"}}"#);
    let hs = [
        ("Authorization", "Bearer operator-secret-1"),
        ("Content-Type", "application/json"),
        ("X-Bot-Csrf", csrf.as_str()),
    ];
    flag.store(false, SeqCst);
    let blocked = raw(
        s.addr,
        "POST",
        "/v1/admin/approvals/ap-1/decide",
        None,
        &hs,
        Some(&body),
    )
    .await;
    assert_eq!(
        (blocked.status, blocked.body.as_str()),
        (503, r#"{"error":"unavailable"}"#)
    );
    assert!(
        s.fake.decisions().is_empty(),
        "옛 집합에서는 브로커까지 가지 않는다"
    );
    let hold = raw(
        s.addr,
        "POST",
        "/v1/admin/holds/personal/r-4/c-9/resolve",
        None,
        &hs,
        Some(r#"{"verdict":"confirmed","receipt":"r","effect":"e-2","hold_id":"h-e-2"}"#),
    )
    .await;
    assert_eq!(hold.status, 503);
    assert!(s.fake.resolutions().is_empty());
    flag.store(true, SeqCst);
    let ok = raw(
        s.addr,
        "POST",
        "/v1/admin/approvals/ap-1/decide",
        None,
        &hs,
        Some(&body),
    )
    .await;
    assert_eq!(ok.status, 200, "{}", ok.body);
}

#[tokio::test]
async fn a_decision_committed_before_the_set_changed_is_not_applied_twice_on_retry() {
    let flag = Arc::new(AtomicBool::new(true));
    let calls = Arc::new(AtomicUsize::new(0));
    let (f, c) = (flag.clone(), calls.clone());
    // 판정의 핸들러 전 확인(2번째 호출)은 통과시키고 응답 직전 확인(3번째)에서 닫는다.
    let s = start_cfg(Fake::demo(), move |cfg| {
        cfg.fence(move || {
            let n = c.fetch_add(1, SeqCst);
            let v = f.load(SeqCst) && n != 2;
            Box::pin(async move { v })
        })
    })
    .await;
    let csrf = csrf(&s, OPERATOR).await; // 확인 1번째
    let seen = bot_admin::json::hex(&bot_admin::fake::digest("c-1:agenda.delete"));
    let body = format!(r#"{{"allow":true,"seen":"{seen}"}}"#);
    let hs = [
        ("Authorization", "Bearer operator-secret-1"),
        ("Content-Type", "application/json"),
        ("X-Bot-Csrf", csrf.as_str()),
    ];
    let first = raw(
        s.addr,
        "POST",
        "/v1/admin/approvals/ap-1/decide",
        None,
        &hs,
        Some(&body),
    )
    .await;
    assert_eq!(
        first.status, 503,
        "응답 직전 확인에서 닫혔다: {}",
        first.body
    );
    assert_eq!(s.fake.decisions().len(), 1, "판정은 이미 커밋됐다");
    let again = raw(
        s.addr,
        "POST",
        "/v1/admin/approvals/ap-1/decide",
        None,
        &hs,
        Some(&body),
    )
    .await;
    assert_eq!(
        (again.status, again.json()["repeated"].as_bool()),
        (200, Some(true)),
        "다시 보내면 기존 결과를 돌려준다"
    );
    assert_eq!(s.fake.decisions().len(), 1, "두 번 적용되지 않는다");
}

#[tokio::test]
async fn static_screen_assets_and_failed_logins_never_consult_the_hook() {
    let (s, _, calls) = fenced(false).await;
    for url in ["/ui", "/ui/app.js", "/ui/holdpin.js"] {
        assert_eq!(
            get(&s, OPERATOR, url).await.status,
            200,
            "{url}: 데이터가 없는 자산은 항상 열린다"
        );
    }
    assert_eq!(calls.load(SeqCst), 0);
    // 인증을 통과하지 못하면 훅까지 가지 않고 평소 응답이다.
    let none = raw(s.addr, "GET", "/v1/admin/runs", None, &[], None).await;
    assert_eq!(none.status, 401);
    let bad_host = raw(
        s.addr,
        "GET",
        "/v1/admin/runs",
        Some("evil.example"),
        &[("Authorization", "Bearer operator-secret-1")],
        None,
    )
    .await;
    assert_eq!(bad_host.status, 421);
    assert_eq!(calls.load(SeqCst), 0);
}

#[tokio::test]
async fn a_hook_that_never_answers_closes_the_door_within_the_time_limit() {
    let s = start_cfg(Fake::demo(), |cfg| {
        cfg.fence(|| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                true
            })
        })
        .fence_timeout(Duration::from_millis(100))
    })
    .await;
    let started = Instant::now();
    let r = get(&s, OPERATOR, "/v1/admin/status").await;
    assert_eq!(
        (r.status, r.body.as_str()),
        (503, r#"{"error":"unavailable"}"#)
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}
