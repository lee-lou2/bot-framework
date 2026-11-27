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
    assert_eq!(upstream.send(upstream_request()).await.unwrap().status, 200);
    assert!(upstream.stream(upstream_request()).await.is_ok());
    assert_eq!(fake.requests().len(), 2);

    active.set(Some("set-2"));
    assert!(matches!(
        upstream.send(upstream_request()).await,
        Err(Error::Stale)
    ));
    assert!(matches!(
        upstream.stream(upstream_request()).await,
        Err(Error::Stale)
    ));
    assert_eq!(
        fake.requests().len(),
        2,
        "stale process must not send new requests"
    );

    active.set(None);
    assert!(upstream.send(upstream_request()).await.is_err());
    assert_eq!(fake.requests().len(), 2);
}

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
    for n in 0..8 {
        let chunk = stream.body.next().await.unwrap().expect("chunk");
        assert_eq!(chunk, format!("event: ping\ndata: {n}\n\n").into_bytes());
    }
    active.set(Some("set-2"));
    let mut after = 0;
    let ended = loop {
        match stream.body.next().await {
            Ok(Some(_)) => after += 1,
            other => break other,
        }
    };
    assert!(matches!(ended, Err(Error::Stale)), "{ended:?}");
    assert!(after < 10, "too many chunks flowed before cut: {after}");
}

#[derive(Clone, Copy)]
enum Mode {
    Same,
    Down,
    Hang,
    Late(u64),
}

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

async fn drain(
    stream: &mut UpstreamStream,
    since: tokio::time::Instant,
) -> (Result<Option<Vec<u8>>, Error>, usize, Duration, Duration) {
    let (mut chunks, mut last) = (0, Duration::ZERO);
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
    .expect("stream did not terminate");
    (outcome, chunks, last, since.elapsed())
}

#[tokio::test]
async fn an_unverifiable_stream_gets_a_grace_and_a_recovered_check_resets_it() {
    let check = Scripted::new();
    let fake = Arc::new(FakeUpstream::new());
    slow_stream(&fake, 200, 20);
    let upstream = guarded(&fake, check.clone(), 20, 300);
    let mut stream = upstream.stream(upstream_request()).await.unwrap();
    check.set(Mode::Down);
    for _ in 0..5 {
        assert!(stream.body.next().await.unwrap().is_some());
    }
    check.set(Mode::Same);
    for _ in 0..30 {
        assert!(stream.body.next().await.unwrap().is_some());
    }
    check.set(Mode::Down);
    let (ended, chunks, _, _) = drain(&mut stream, tokio::time::Instant::now()).await;
    assert!(matches!(ended, Err(Error::Unavailable)), "{ended:?}");
    assert!(
        chunks > 3,
        "stream should continue during a fresh grace window: {chunks}"
    );
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
    check.set(Mode::Down);
    let flipped = tokio::time::Instant::now();
    let (ended, chunks, last, cut) = drain(&mut stream, flipped).await;
    assert!(matches!(ended, Err(Error::Unavailable)), "{ended:?}");
    let (every, grace) = (Duration::from_millis(every), Duration::from_millis(grace));
    let slack = Duration::from_millis(250);
    assert!(chunks > 5, "should emit chunks during grace: {chunks}");
    assert!(
        cut >= grace - Duration::from_millis(15),
        "stream cut before deadline: {cut:?}"
    );
    assert!(
        last <= every + grace + slack,
        "chunk emitted after deadline: {last:?}"
    );
    assert!(
        cut <= every + grace + slack,
        "stream was not cut at deadline: {cut:?}"
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
    check.set(Mode::Down);
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(stream.body.next().await.unwrap().is_some());
    tokio::time::sleep(Duration::from_millis(grace + 150)).await;
    let late = stream.body.next().await;
    assert!(
        matches!(late, Err(Error::Unavailable)),
        "chunk emitted past deadline: {late:?}"
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
    let (ended, _, _, cut) = drain(&mut stream, flipped).await;
    assert!(matches!(ended, Err(Error::Unavailable)), "{ended:?}");
    assert!(
        cut <= Duration::from_millis(every + grace + 250),
        "hanging check stretched the grace window: {cut:?}"
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
    check.set(Mode::Late(600));
    let flipped = tokio::time::Instant::now();
    let (ended, _, _, cut) = drain(&mut stream, flipped).await;
    assert!(matches!(ended, Err(Error::Unavailable)), "{ended:?}");
    assert!(
        cut < Duration::from_millis(600),
        "waited for and accepted a late success: {cut:?}"
    );
}

#[tokio::test]
async fn a_new_request_waits_for_the_check_only_up_to_the_grace_and_sends_nothing_when_it_fails() {
    let check = Scripted::new();
    let fake = Arc::new(FakeUpstream::new());
    let upstream = guarded(&fake, check.clone(), 30, 200);
    check.set(Mode::Hang);
    let began = tokio::time::Instant::now();
    let held = tokio::time::timeout(Duration::from_secs(3), upstream.stream(upstream_request()))
        .await
        .expect("request hung");
    assert!(matches!(held, Err(Error::Unavailable)));
    assert!(began.elapsed() < Duration::from_secs(1));
    let sent = tokio::time::timeout(Duration::from_secs(3), upstream.send(upstream_request()))
        .await
        .expect("request hung");
    assert!(matches!(sent, Err(Error::Unavailable)));

    check.set(Mode::Late(600));
    let began = tokio::time::Instant::now();
    assert!(matches!(
        upstream.stream(upstream_request()).await,
        Err(Error::Unavailable)
    ));
    assert!(
        began.elapsed() < Duration::from_millis(550),
        "waited for a late success"
    );

    check.set(Mode::Down);
    assert!(matches!(
        upstream.stream(upstream_request()).await,
        Err(Error::Unavailable)
    ));
    assert!(
        fake.requests().is_empty(),
        "request reached upstream without verification"
    );
}
