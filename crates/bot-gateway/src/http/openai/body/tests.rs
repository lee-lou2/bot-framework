use super::*;
use axum::body::Bytes;
use futures_util::stream;
use std::{convert::Infallible, time::Duration};

type Part = Result<Bytes, std::io::Error>;

fn chunk(len: usize) -> Part {
    Ok(Bytes::from(vec![b'x'; len]))
}

fn broken() -> Part {
    Err(std::io::Error::other("disconnected"))
}

fn body(parts: Vec<Part>) -> Body {
    Body::from_stream(stream::iter(parts))
}

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
    assert_eq!(
        outcome(body(vec![chunk(5)]), 10, Some(500)).await,
        Err("request_too_large")
    );
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
    assert_eq!(
        outcome(body(vec![chunk(5), broken()]), 10, Some(500)).await,
        Err("request_too_large")
    );
    assert_eq!(
        outcome(body(vec![chunk(20), broken()]), 10, None).await,
        Err("request_too_large")
    );
    assert_eq!(
        outcome(body(vec![chunk(5), broken()]), 10, None).await,
        Err("invalid_request")
    );
}

#[tokio::test]
async fn a_slow_stream_keeps_its_413_once_the_overrun_is_known() {
    assert_eq!(
        outcome(stalled(vec![chunk(20)]), 10, None).await,
        Err("request_too_large")
    );
    assert_eq!(
        outcome(stalled(vec![chunk(5)]), 10, Some(500)).await,
        Err("request_too_large")
    );
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
