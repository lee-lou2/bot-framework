use super::*;
use axum::body::Bytes;
use futures_util::stream;
use std::{convert::Infallible, time::Duration};

type Part = Result<Bytes, std::io::Error>;

fn chunk(len: usize) -> Part {
    Ok(Bytes::from(vec![b'x'; len]))
}

fn broken() -> Part {
    Err(std::io::Error::other("끊김"))
}

fn body(parts: Vec<Part>) -> Body {
    Body::from_stream(stream::iter(parts))
}

/// 첫 조각들 뒤에 영원히 오지 않는 본문
fn stalled(parts: Vec<Part>) -> Body {
    Body::from_stream(stream::iter(parts).chain(stream::pending()))
}

fn soon() -> Instant {
    Instant::now() + Duration::from_millis(150)
}

fn headers(length: Option<usize>) -> HeaderMap {
    let mut map = HeaderMap::new();
    if let Some(length) = length {
        map.insert(header::CONTENT_LENGTH, length.to_string().parse().unwrap());
    }
    map
}

async fn outcome(body: Body, limit: usize, length: Option<usize>) -> Result<usize, &'static str> {
    read(body, limit, &headers(length), soon())
        .await
        .map(|data| data.len())
        .map_err(|failure| failure.code)
}

#[tokio::test]
async fn a_body_within_the_limit_is_read_whole() {
    assert_eq!(
        outcome(body(vec![chunk(10), chunk(5)]), 15, Some(15)).await,
        Ok(15)
    );
    assert_eq!(outcome(body(vec![]), 15, None).await, Ok(0));
}

#[tokio::test]
async fn the_header_and_the_bytes_actually_received_are_both_counted() {
    // 길이 헤더가 한도를 넘는다
    assert_eq!(
        outcome(body(vec![chunk(5)]), 10, Some(500)).await,
        Err("request_too_large")
    );
    // 헤더는 작다고 하지만(또는 없지만) 받은 바이트가 한도를 넘는다
    assert_eq!(
        outcome(body(vec![chunk(6), chunk(6)]), 10, Some(5)).await,
        Err("request_too_large")
    );
    assert_eq!(
        outcome(body(vec![chunk(6), chunk(6)]), 10, None).await,
        Err("request_too_large")
    );
}

#[tokio::test]
async fn a_broken_stream_keeps_its_413_once_the_overrun_is_known() {
    // 헤더로 초과를 알았다: 이어진 읽기 오류가 413을 400으로 덮지 않는다
    assert_eq!(
        outcome(body(vec![chunk(5), broken()]), 10, Some(500)).await,
        Err("request_too_large")
    );
    // 받은 바이트로 초과를 알았다
    assert_eq!(
        outcome(body(vec![chunk(20), broken()]), 10, None).await,
        Err("request_too_large")
    );
    // 초과를 모르는 채 끊기면 400
    assert_eq!(
        outcome(body(vec![chunk(5), broken()]), 10, None).await,
        Err("invalid_request")
    );
}

#[tokio::test]
async fn a_slow_stream_keeps_its_413_once_the_overrun_is_known() {
    // 초과를 알았는데 남은 본문이 느리다: 408이 아니라 413
    assert_eq!(
        outcome(stalled(vec![chunk(20)]), 10, None).await,
        Err("request_too_large")
    );
    assert_eq!(
        outcome(stalled(vec![chunk(5)]), 10, Some(500)).await,
        Err("request_too_large")
    );
    // 초과를 모르는 느린 본문은 408
    assert_eq!(
        outcome(stalled(vec![chunk(5)]), 10, None).await,
        Err("request_timeout")
    );
    assert_eq!(
        outcome(stalled(vec![]), 10, Some(5)).await,
        Err("request_timeout")
    );
}

#[tokio::test]
async fn draining_an_endless_overrun_stops_after_a_bounded_amount() {
    let endless = Body::from_stream(stream::repeat_with(|| -> Result<Bytes, Infallible> {
        Ok(Bytes::from(vec![b'x'; 64 * 1024]))
    }));
    let started = Instant::now();
    let result = read(
        endless,
        1024,
        &headers(None),
        Instant::now() + Duration::from_secs(10),
    )
    .await;
    assert_eq!(result.unwrap_err().code, "request_too_large");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}
