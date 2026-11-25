use super::*;
use futures_util::StreamExt;
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

/// 확인 결과를 차례로 돌려주는 가짜 경계(비면 통과). `delay`만큼 기다린 뒤 답한다.
struct Fake {
    verdicts: Mutex<VecDeque<Result<(), Failure>>>,
    calls: AtomicUsize,
    delay: Duration,
}

impl Fake {
    fn new(verdicts: Vec<Result<(), Failure>>) -> Arc<Self> {
        Arc::new(Self {
            verdicts: Mutex::new(verdicts.into()),
            calls: AtomicUsize::new(0),
            delay: Duration::ZERO,
        })
    }

    fn slow(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            verdicts: Mutex::new(VecDeque::new()),
            calls: AtomicUsize::new(0),
            delay,
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Boundary for Fake {
    fn admit(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            self.verdicts.lock().unwrap().pop_front().unwrap_or(Ok(()))
        })
    }
}

fn queue(items: Vec<Item>) -> mpsc::Receiver<Item> {
    let (tx, rx) = mpsc::channel(items.len().max(1));
    for item in items {
        assert!(tx.try_send(item).is_ok(), "큐에 들어가야 해요");
    }
    rx
}

fn data(text: &str) -> Item {
    Item::Data(text.to_owned())
}

fn beat() -> Item {
    Item::Beat(": keep-alive\n\n".to_owned())
}

fn later(ms: u64) -> Instant {
    Instant::now() + Duration::from_millis(ms)
}

async fn collect(stream: impl Stream<Item = Result<Bytes, Infallible>>) -> Vec<String> {
    stream
        .map(|chunk| String::from_utf8(chunk.unwrap().to_vec()).unwrap())
        .collect()
        .await
}

fn failure(code: &'static str) -> Failure {
    match code {
        "access_denied" => Failure::access_denied(),
        "invalid_api_key" => Failure::invalid_api_key(),
        _ => Failure::model_not_found(),
    }
}

fn code_of(event: &str) -> String {
    let value: serde_json::Value =
        serde_json::from_str(event.strip_prefix("data: ").unwrap().trim_end()).unwrap();
    value["error"]["code"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn every_data_item_is_checked_when_it_is_taken_and_a_beat_needs_no_check() {
    let fake = Fake::new(vec![]);
    let out = collect(gated(
        queue(vec![data("a"), beat(), data("b"), data("c")]),
        later(5_000),
        fake.clone(),
        None,
        None,
    ))
    .await;
    assert_eq!(out, ["a", ": keep-alive\n\n", "b", "c"]);
    assert_eq!(
        fake.calls(),
        3,
        "본문 조각마다 한 번씩(주석은 확인하지 않는다)"
    );
}

#[tokio::test]
async fn a_failed_check_discards_that_item_and_everything_queued_and_ends_with_one_error_event() {
    let fake = Fake::new(vec![Ok(()), Err(failure("access_denied"))]);
    let out = collect(gated(
        queue(vec![
            data("first"),
            data("SECRET-second"),
            data("SECRET-third"),
            beat(),
        ]),
        later(5_000),
        fake.clone(),
        None,
        None,
    ))
    .await;
    assert_eq!(out.len(), 2, "{out:?}");
    assert_eq!(out[0], "first");
    assert_eq!(code_of(&out[1]), "access_denied");
    assert!(!out.concat().contains("SECRET"), "{out:?}");
    assert_eq!(fake.calls(), 2, "버린 조각은 더 확인하지 않는다");
}

#[tokio::test]
async fn the_reason_of_the_failed_check_is_what_the_error_event_says() {
    for code in ["access_denied", "invalid_api_key", "model_not_found"] {
        let fake = Fake::new(vec![Err(failure(code))]);
        let out = collect(gated(
            queue(vec![data("x")]),
            later(5_000),
            fake,
            None,
            None,
        ))
        .await;
        assert_eq!(out.len(), 1);
        assert_eq!(code_of(&out[0]), code);
    }
}

#[tokio::test]
async fn after_the_deadline_no_data_passes_and_one_unknown_outcome_event_closes_the_stream() {
    let fake = Fake::new(vec![]);
    let out = collect(gated(
        queue(vec![data("SECRET-a"), data("SECRET-b")]),
        Instant::now() - Duration::from_millis(1),
        fake.clone(),
        None,
        None,
    ))
    .await;
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(code_of(&out[0]), "request_outcome_unknown");
    assert_eq!(fake.calls(), 0, "기한을 넘겼으면 확인할 것도 없다");
    // 기한 뒤의 주석은 쓰지 않고 닫는다
    let out = collect(gated(
        queue(vec![beat(), data("SECRET")]),
        Instant::now() - Duration::from_millis(1),
        Fake::new(vec![]),
        None,
        None,
    ))
    .await;
    assert!(out.is_empty(), "{out:?}");
}

#[tokio::test]
async fn a_check_that_only_finishes_after_the_deadline_hands_nothing_over() {
    let fake = Fake::slow(Duration::from_millis(200));
    let out = collect(gated(
        queue(vec![data("SECRET")]),
        later(80),
        fake,
        None,
        None,
    ))
    .await;
    assert_eq!(out.len(), 1, "{out:?}");
    assert_eq!(code_of(&out[0]), "request_outcome_unknown");
    assert!(!out[0].contains("SECRET"));
}

#[tokio::test]
async fn the_probe_sees_exactly_what_crosses_the_boundary() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let probe: Probe = {
        let seen = seen.clone();
        Arc::new(move |kind, bytes| seen.lock().unwrap().push((kind, bytes)))
    };
    let fake = Fake::new(vec![Ok(()), Err(failure("access_denied"))]);
    let out = collect(gated(
        queue(vec![data("12345"), beat(), data("SECRET")]),
        later(5_000),
        fake,
        Some(probe),
        None,
    ))
    .await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert_eq!(seen[0], (Passed::Data, 5));
    assert_eq!(seen[1].0, Passed::Beat);
    assert_eq!(seen[2].0, Passed::Error);
    assert_eq!(seen[2].1, out[2].len());
    assert!(!out.concat().contains("SECRET"));
}

#[tokio::test]
async fn closing_the_stream_releases_a_producer_waiting_for_room() {
    let (tx, rx) = mpsc::channel::<Item>(BUFFER);
    let fake = Fake::new(vec![Err(failure("access_denied"))]);
    let mut stream = Box::pin(gated(rx, later(5_000), fake, None, None));
    let producer = tokio::spawn(async move {
        let mut sent = 0;
        while tx.send(data("x")).await.is_ok() {
            sent += 1;
        }
        sent
    });
    // 첫 확인이 실패해 오류 사건으로 닫힌다: 큐가 닫혀 생산자가 풀려난다
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(
        code_of(std::str::from_utf8(&first).unwrap()),
        "access_denied"
    );
    assert!(stream.next().await.is_none());
    let sent = tokio::time::timeout(Duration::from_secs(2), producer)
        .await
        .expect("생산자가 풀려나야 해요")
        .unwrap();
    assert!(sent <= BUFFER + 2, "{sent}");
}

fn one_slot() -> (Arc<tokio::sync::Semaphore>, OwnedSemaphorePermit) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = semaphore.clone().try_acquire_owned().expect("한 자리");
    (semaphore, permit)
}

#[tokio::test]
async fn the_connection_slot_stays_taken_until_the_body_ends_even_when_the_producer_is_long_gone() {
    let (semaphore, permit) = one_slot();
    // 생산자는 조각을 다 넣고 사라졌다(큐의 송신 쪽이 이미 닫혔다). 본문은 아직 확인을 기다린다.
    let fake = Fake::slow(Duration::from_millis(300));
    let mut stream = Box::pin(gated(
        queue(vec![data("a"), data("b")]),
        later(5_000),
        fake,
        None,
        Some(permit),
    ));
    let first = tokio::spawn(async move {
        let item = stream.next().await;
        (item.is_some(), stream)
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        semaphore.available_permits(),
        0,
        "확인을 기다리는 동안 자리를 돌려주면 안 돼요"
    );
    let (got, mut stream) = first.await.unwrap();
    assert!(got);
    assert_eq!(
        semaphore.available_permits(),
        0,
        "본문이 아직 끝나지 않았다"
    );
    while stream.next().await.is_some() {}
    assert_eq!(semaphore.available_permits(), 1, "본문이 끝나면 돌려준다");
}

#[tokio::test]
async fn the_connection_slot_is_returned_when_the_body_is_dropped_or_closes_on_an_error() {
    // 버려지면(상대가 끊었다) 돌려준다
    let (semaphore, permit) = one_slot();
    let stream = Box::pin(gated(
        queue(vec![data("a"), data("b")]),
        later(5_000),
        Fake::new(vec![]),
        None,
        Some(permit),
    ));
    assert_eq!(semaphore.available_permits(), 0);
    drop(stream);
    assert_eq!(semaphore.available_permits(), 1);
    // 오류 사건으로 닫혀도 본문이 끝날 때까지는 쥐고, 끝나면 돌려준다
    let (semaphore, permit) = one_slot();
    let mut stream = Box::pin(gated(
        queue(vec![data("a")]),
        later(5_000),
        Fake::new(vec![Err(failure("access_denied"))]),
        None,
        Some(permit),
    ));
    assert!(stream.next().await.is_some());
    assert_eq!(
        semaphore.available_permits(),
        0,
        "오류 사건을 넘긴 뒤에도 본문이 끝나기 전이다"
    );
    assert!(stream.next().await.is_none());
    assert_eq!(semaphore.available_permits(), 1);
}

#[tokio::test]
async fn a_full_queue_gives_up_at_the_deadline_instead_of_waiting_forever() {
    let (tx, _rx) = mpsc::channel::<Item>(1);
    let mut sink = Sink {
        tx,
        last: Instant::now(),
        hard: later(120),
        every: Duration::from_secs(10),
    };
    assert!(sink.data("a".into()).await, "자리가 있으면 바로 들어간다");
    let started = Instant::now();
    assert!(
        !sink.data("b".into()).await,
        "아무도 꺼내 가지 않으면 기한에 포기한다"
    );
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(90) && waited < Duration::from_secs(2),
        "{waited:?}"
    );
}

#[tokio::test]
async fn a_closed_queue_is_noticed_at_once_and_keep_alive_is_due_only_after_the_interval() {
    let (tx, rx) = mpsc::channel::<Item>(2);
    let mut sink = Sink {
        tx,
        last: Instant::now(),
        hard: later(5_000),
        every: Duration::from_millis(80),
    };
    // 간격 안에는 쓰지 않는다
    assert!(sink.keep_alive().await);
    assert!(sink.until_keep_alive() > Duration::ZERO);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(sink.until_keep_alive(), Duration::ZERO);
    assert!(sink.keep_alive().await, "간격이 지나면 주석 하나를 쓴다");
    drop(rx);
    // 받는 쪽이 사라졌다: 기다리지 않고 알아챈다
    tokio::time::timeout(Duration::from_millis(200), sink.gone())
        .await
        .expect("곧바로 알아채야 해요");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!sink.keep_alive().await);
    assert!(!sink.data("x".into()).await);
}
