//! Bounded, deadline-enforced OpenAI request body reader with controlled 413 draining.
use super::error::Failure;
use axum::{
    body::Body,
    http::{HeaderMap, header},
};
use futures_util::StreamExt;
use tokio::time::{Instant, timeout_at};

/// Drain up to this many excess bytes after exceeding the limit so clients read 413 instead of a reset.
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
            // Once over-limit is known, preserve 413 even if draining times out or errors.
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
                Failure::invalid("Failed to read request body")
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
