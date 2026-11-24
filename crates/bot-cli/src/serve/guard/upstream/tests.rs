use super::*;
use bot_broker::{
    resolve::HostPort,
    testing::{End, FakeUpstream},
};
use bot_gateway::testing::ports::MemoryActive;

fn upstream_request() -> UpstreamRequest {
    UpstreamRequest {
        endpoint: HostPort {
            host: "api.main.example".into(),
            port: 443,
        },
        path: "/v1/messages".into(),
        headers: Vec::new(),
        body: b"{}".to_vec(),
    }
}

fn guarded(
    fake: &Arc<FakeUpstream>,
    active: Arc<dyn ActiveCheck>,
    every: u64,
    grace: u64,
) -> GuardedUpstream {
    GuardedUpstream::with_timing(
        fake.clone(),
        active,
        Duration::from_millis(every),
        Duration::from_millis(grace),
    )
}

#[tokio::test]
async fn a_stale_process_sends_nothing_to_the_model_upstream() {
    let active = MemoryActive::new(Some("set-1"));
    let fake = Arc::new(FakeUpstream::new());
    let upstream = guarded(&fake, active.check("set-1"), 20, 1000);
    // 같은 판본: 상류까지 간다
    assert_eq!(upstream.send(upstream_request()).await.unwrap().status, 200);
    assert!(upstream.stream(upstream_request()).await.is_ok());
    assert_eq!(fake.requests().len(), 2);
    // 다른 프로세스가 집합(상류 목적지)을 바꿨다: 비스트리밍도 스트리밍도 상류에 닿지 않는다
    active.set(Some("set-2"));
    assert!(matches!(
        upstream.send(upstream_request()).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        upstream.stream(upstream_request()).await,
        Err(Error::Stale)
    ));
    assert_eq!(fake.requests().len(), 2, "구판은 새 요청을 보내지 않아요");
    // 집합이 아직 게시되지 않았거나 읽지 못해도 보내지 않는다
    active.set(None);
    assert!(upstream.send(upstream_request()).await.is_err());
    assert_eq!(fake.requests().len(), 2);
}

/// 조각 둘을 천천히 흘리는 응답을 상류에 적어 둔다(조각 사이 `gap`)
fn slow_stream(fake: &FakeUpstream, chunks: usize, gap: u64) {
    let parts: Vec<Vec<u8>> = (0..chunks)
        .map(|n| format!("event: ping\ndata: {n}\n\n").into_bytes())
        .collect();
    let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
    fake.respond_slowly(200, &refs, Duration::from_millis(gap), End::Eof);
}

#[tokio::test]
async fn an_open_stream_is_cut_when_the_activation_changes_and_loses_no_chunk_before() {
    let active = MemoryActive::new(Some("set-1"));
    let fake = Arc::new(FakeUpstream::new());
    slow_stream(&fake, 40, 25);
    let upstream = guarded(&fake, active.check("set-1"), 40, 1000);
    let mut stream = upstream.stream(upstream_request()).await.unwrap();
    // 판본이 같은 동안은 조각이 순서대로 하나도 빠지지 않고 나온다(확인이 읽기를 끊지 않는다)
    for n in 0..8 {
        let chunk = stream.body.next().await.unwrap().expect("조각");
        assert_eq!(chunk, format!("event: ping\ndata: {n}\n\n").into_bytes());
    }
    active.set(Some("set-2"));
    // 다음 확인(40ms 안팎)에서 끊긴다: 40조각을 다 받기 전에 Stale이다
    let mut after = 0;
    let ended = loop {
        match stream.body.next().await {
            Ok(Some(_)) => after += 1,
            other => break other,
        }
    };
    assert!(matches!(ended, Err(Error::Stale)), "{ended:?}");
    assert!(after < 10, "끊기기까지 너무 많이 흘렀어요: {after}");
}

/// 판본 확인이 하는 일
#[derive(Clone, Copy)]
enum Mode {
    /// 같은 판본이다
    Same,
    /// 저장소를 읽지 못한다
    Down,
    /// 끝나지 않는다(저장소가 멎었다)
    Hang,
    /// 이만큼(ms) 걸린 뒤 같은 판본이라고 답한다(늦은 성공)
    Late(u64),
}

/// 정한 대로 답하는 확인기. 모드는 시험이 바꾼다.
struct Scripted(std::sync::Mutex<Mode>);

impl Scripted {
    fn new() -> Arc<Self> {
        Arc::new(Self(std::sync::Mutex::new(Mode::Same)))
    }

    fn set(&self, mode: Mode) {
        *self.0.lock().unwrap() = mode;
    }
}

impl ActiveCheck for Scripted {
    fn verify(&self) -> Reply<'_, ()> {
        let mode = *self.0.lock().unwrap();
        Box::pin(async move {
            match mode {
                Mode::Same => Ok(()),
                Mode::Down => Err(Error::Unavailable),
                Mode::Hang => std::future::pending().await,
                Mode::Late(ms) => {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    Ok(())
                }
            }
        })
    }
}

/// 끝났거나(오류) 마감을 넘기도록 조각이 계속 나오는 동안 읽는다. (오류 또는 끝, 나온 조각 수, 마지막 조각이 나온 때, 끝난 때)는 `since`부터의 시간이다.
async fn drain(
    stream: &mut UpstreamStream,
    since: tokio::time::Instant,
) -> (Result<Option<Vec<u8>>, Error>, usize, Duration, Duration) {
    let (mut chunks, mut last) = (0, Duration::ZERO);
    // 끝나지 않으면 시험이 멈추지 않고 실패한다
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match stream.body.next().await {
                Ok(Some(_)) => {
                    chunks += 1;
                    last = since.elapsed();
                }
                other => break other,
            }
        }
    })
    .await
    .expect("스트림이 끝나지 않았어요");
    (outcome, chunks, last, since.elapsed())
}

#[tokio::test]
async fn an_unverifiable_stream_gets_a_grace_and_a_recovered_check_resets_it() {
    let check = Scripted::new();
    let fake = Arc::new(FakeUpstream::new());
    slow_stream(&fake, 200, 20);
    let upstream = guarded(&fake, check.clone(), 20, 300);
    let mut stream = upstream.stream(upstream_request()).await.unwrap();
    // 유예 안에서 확인이 실패하다 돌아오면 스트림은 이어진다
    check.set(Mode::Down);
    for _ in 0..5 {
        assert!(stream.body.next().await.unwrap().is_some());
    }
    check.set(Mode::Same);
    for _ in 0..30 {
        assert!(stream.body.next().await.unwrap().is_some());
    }
    // 돌아온 확인이 마감을 비웠다: 새 구간은 처음부터 다시 센다(앞의 실패가 쌓이지 않는다)
    check.set(Mode::Down);
    let (ended, chunks, _, _) = drain(&mut stream, tokio::time::Instant::now()).await;
    assert!(matches!(ended, Err(Error::Unavailable)), "{ended:?}");
    assert!(chunks > 3, "새 구간의 유예 동안은 흘러야 해요: {chunks}");
}

#[tokio::test]
async fn no_chunk_is_emitted_after_the_single_deadline_and_the_cut_is_not_early() {
    let check = Scripted::new();
    let fake = Arc::new(FakeUpstream::new());
    slow_stream(&fake, 400, 5);
    let (every, grace) = (30, 250);
    let upstream = guarded(&fake, check.clone(), every, grace);
    let mut stream = upstream.stream(upstream_request()).await.unwrap();
    for _ in 0..4 {
        assert!(stream.body.next().await.unwrap().is_some());
    }
    // 확인이 계속 실패한다. 조각은 계속 도착하지만 마감(처음 실패한 확인이 마땅했던 시각 + 유예)을 넘기면 하나도 내보내지 않는다
    check.set(Mode::Down);
    let flipped = tokio::time::Instant::now();
    let (ended, chunks, last, cut) = drain(&mut stream, flipped).await;
    assert!(matches!(ended, Err(Error::Unavailable)), "{ended:?}");
    let (every, grace) = (Duration::from_millis(every), Duration::from_millis(grace));
    let slack = Duration::from_millis(250);
    assert!(chunks > 5, "유예 동안은 흘러야 해요: {chunks}");
    assert!(
        cut >= grace - Duration::from_millis(15),
        "마감 전에 끊겼어요: {cut:?}"
    );
    assert!(
        last <= every + grace + slack,
        "마감 뒤에도 조각을 내보냈어요: {last:?}"
    );
    assert!(
        cut <= every + grace + slack,
        "마감에 끊기지 않았어요: {cut:?}"
    );
}

#[tokio::test]
async fn a_chunk_that_is_ready_after_the_deadline_is_dropped_not_emitted() {
    let check = Scripted::new();
    let fake = Arc::new(FakeUpstream::new());
    slow_stream(&fake, 50, 0);
    let (every, grace) = (30, 200);
    let upstream = guarded(&fake, check.clone(), every, grace);
    let mut stream = upstream.stream(upstream_request()).await.unwrap();
    // 확인이 마땅한 때가 되도록 기다린 뒤 실패하게 한다: 다음 읽기가 확인을 먼저 하고(실패) 마감을 정한 다음 준비된 조각을 준다
    check.set(Mode::Down);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(stream.body.next().await.unwrap().is_some());
    // 읽지 않고 마감을 넘긴다. 조각은 이미 준비돼 있지만 마감 뒤에는 하나도 내보내지 않는다
    tokio::time::sleep(Duration::from_millis(grace + 150)).await;
    let late = stream.body.next().await;
    assert!(
        matches!(late, Err(Error::Unavailable)),
        "마감 뒤에 조각을 내보냈어요: {late:?}"
    );
}

#[tokio::test]
async fn a_check_that_never_ends_cannot_stretch_the_grace() {
    let check = Scripted::new();
    let fake = Arc::new(FakeUpstream::new());
    slow_stream(&fake, 400, 10);
    let (every, grace) = (30, 250);
    let upstream = guarded(&fake, check.clone(), every, grace);
    let mut stream = upstream.stream(upstream_request()).await.unwrap();
    check.set(Mode::Hang);
    let flipped = tokio::time::Instant::now();
    // 확인이 끝나지 않아도 마감(`every + grace` 안팎)에 끊긴다 — 조회 자체가 마감에 묶인다
    let (ended, _, _, cut) = drain(&mut stream, flipped).await;
    assert!(matches!(ended, Err(Error::Unavailable)), "{ended:?}");
    assert!(
        cut <= Duration::from_millis(every + grace + 250),
        "끝나지 않는 확인이 유예를 늘렸어요: {cut:?}"
    );
}

#[tokio::test]
async fn a_success_that_arrives_after_the_deadline_is_not_accepted() {
    let check = Scripted::new();
    let fake = Arc::new(FakeUpstream::new());
    slow_stream(&fake, 400, 10);
    let (every, grace) = (30, 200);
    let upstream = guarded(&fake, check.clone(), every, grace);
    let mut stream = upstream.stream(upstream_request()).await.unwrap();
    // 확인이 마감(200ms)보다 오래(600ms) 걸려 성공을 답한다: 그 성공은 늦었으니 받지 않고 마감에 끊는다
    check.set(Mode::Late(600));
    let flipped = tokio::time::Instant::now();
    let (ended, _, _, cut) = drain(&mut stream, flipped).await;
    assert!(matches!(ended, Err(Error::Unavailable)), "{ended:?}");
    assert!(
        cut < Duration::from_millis(600),
        "늦은 성공을 기다렸다가 받았어요: {cut:?}"
    );
}

#[tokio::test]
async fn a_new_request_waits_for_the_check_only_up_to_the_grace_and_sends_nothing_when_it_fails() {
    let check = Scripted::new();
    let fake = Arc::new(FakeUpstream::new());
    let upstream = guarded(&fake, check.clone(), 30, 200);
    // 끝나지 않는 확인이 새 요청을 붙잡아 두지 못한다
    check.set(Mode::Hang);
    let began = tokio::time::Instant::now();
    let held = tokio::time::timeout(Duration::from_secs(3), upstream.stream(upstream_request()))
        .await
        .expect("요청이 붙잡혔어요");
    assert!(matches!(held, Err(Error::Unavailable)));
    assert!(began.elapsed() < Duration::from_secs(1));
    let sent = tokio::time::timeout(Duration::from_secs(3), upstream.send(upstream_request()))
        .await
        .expect("요청이 붙잡혔어요");
    assert!(matches!(sent, Err(Error::Unavailable)));
    // 마감(200ms)을 넘겨 도착하는 성공은 받지 않는다: 늦게 끝난 확인으로 요청이 나가지 않는다
    check.set(Mode::Late(600));
    let began = tokio::time::Instant::now();
    assert!(matches!(
        upstream.stream(upstream_request()).await,
        Err(Error::Unavailable)
    ));
    assert!(
        began.elapsed() < Duration::from_millis(550),
        "늦은 성공을 기다렸어요"
    );
    // 저장소를 읽지 못해도 보내지 않는다(Stale이 아니라 Unavailable로 알린다)
    check.set(Mode::Down);
    assert!(matches!(
        upstream.stream(upstream_request()).await,
        Err(Error::Unavailable)
    ));
    assert!(
        fake.requests().is_empty(),
        "확인하지 못한 채 상류로 나갔어요"
    );
}
