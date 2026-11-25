//! 요청 본문 읽기. 길이 헤더와 실제로 받은 누적 바이트를 모두 한도에 견준다(청크 전송은 헤더가 없다). 읽는 시간은 `until`까지다.
//!
//! 한도를 넘은 요청도 413으로 끝나야 상대가 그 응답을 읽을 수 있다. 본문을 읽지 않고 응답하면 연결이 재설정돼 SDK는 413 대신 연결 오류(자동 재시도 대상)를 본다.
//! 그래서 한도를 넘은 뒤에도 남은 본문을 `DRAIN_EXTRA`까지만 버리며 읽고 413을 낸다(그 이상 보내는 상대는 응답을 못 읽어도 상관없다).
//! 초과를 이미 확정한 뒤의 읽기 오류·시간 초과는 그 사실을 덮지 못한다: 413이다. 초과를 모르는 채 느리거나 끊긴 본문은 408·400이다.
use super::error::Failure;
use axum::{
    body::Body,
    http::{HeaderMap, header},
};
use futures_util::StreamExt;
use tokio::time::{Instant, timeout_at};

/// 한도를 넘은 뒤 상대가 413을 읽을 수 있도록 버려 주는 최대 바이트
const DRAIN_EXTRA: usize = 1 << 20;

pub(super) async fn read(
    body: Body,
    limit: usize,
    headers: &HeaderMap,
    until: Instant,
) -> Result<Vec<u8>, Failure> {
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    let mut over = declared.is_some_and(|len| len > limit);
    let mut stream = body.into_data_stream();
    let mut data = Vec::new();
    let mut seen = 0usize;
    loop {
        let Ok(next) = timeout_at(until, stream.next()).await else {
            return Err(if over {
                Failure::too_large()
            } else {
                Failure::read_timeout()
            });
        };
        let Some(chunk) = next else {
            break;
        };
        let Ok(chunk) = chunk else {
            return Err(if over {
                Failure::too_large()
            } else {
                Failure::invalid("요청 본문을 읽지 못했어요")
            });
        };
        seen = seen.saturating_add(chunk.len());
        over = over || seen > limit;
        if !over {
            data.extend_from_slice(&chunk);
        } else if seen > limit.saturating_add(DRAIN_EXTRA) {
            break;
        }
    }
    if over {
        Err(Failure::too_large())
    } else {
        Ok(data)
    }
}

#[cfg(test)]
mod tests;
