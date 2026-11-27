//! OpenAI SSE response stream enforcing authorization and deadline checks at each chunk handoff.
use super::{
    chat::{Ctl, Resolved},
    error::Failure,
    render::{self, Identity},
};
use axum::{
    body::{Body, Bytes},
    http::{HeaderValue, StatusCode, header},
    response::Response,
};
use bot_core::api::{ApiRun, ApiState};
use futures_util::{Stream, future::BoxFuture};
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::{
    sync::{OwnedSemaphorePermit, mpsc},
    time::{Instant, timeout_at},
};

/// Keep-alive interval below the 15-second contract bound.
pub const KEEP_ALIVE_EVERY: Duration = Duration::from_secs(10);
/// Small channel buffer so the producer cannot queue far ahead of a stalled reader.
const BUFFER: usize = 2;

pub(super) enum Item {
    Beat(String),
    Data(String),
}

/// Test observation hook for chunks crossing the HTTP body handoff boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Passed {
    Beat,
    Data,
    Error,
}

pub type Probe = Arc<dyn Fn(Passed, usize) + Send + Sync>;

pub(super) trait Boundary: Send + Sync + 'static {
    fn admit(&self) -> BoxFuture<'_, Result<(), Failure>>;
}

struct Gate {
    ctl: Arc<Ctl>,
    basis: ApiRun,
}

impl Boundary for Gate {
    fn admit(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(self.ctl.release(&self.basis))
    }
}

pub(super) struct Sink {
    tx: mpsc::Sender<Item>,
    last: Instant,
    hard: Instant,
    every: Duration,
}

impl Sink {
    pub(super) async fn data(&mut self, text: String) -> bool {
        self.push(Item::Data(text)).await
    }

    pub(super) async fn keep_alive(&mut self) -> bool {
        if self.last.elapsed() >= self.every {
            self.push(Item::Beat(render::KEEP_ALIVE.to_owned())).await
        } else {
            true
        }
    }

    async fn push(&mut self, item: Item) -> bool {
        match timeout_at(self.hard, self.tx.send(item)).await {
            Ok(Ok(())) => {
                self.last = Instant::now();
                true
            }
            _ => false,
        }
    }

    pub(super) fn until_keep_alive(&self) -> Duration {
        self.every.saturating_sub(self.last.elapsed())
    }

    pub(super) async fn gone(&self) {
        self.tx.closed().await;
    }
}

/// Pulls queued chunks and enforces authorization + deadline right at handoff to hyper.
/// Holds the connection `permit` until the response body finishes or is dropped.
pub(super) fn gated(
    rx: mpsc::Receiver<Item>,
    hard: Instant,
    boundary: Arc<dyn Boundary>,
    probe: Option<Probe>,
    permit: Option<OwnedSemaphorePermit>,
) -> impl Stream<Item = Result<Bytes, Infallible>> + Send {
    futures_util::stream::unfold((Some(rx), permit), move |(state, permit)| {
        let (boundary, probe) = (boundary.clone(), probe.clone());
        async move {
            let mut rx = state?;
            let item = rx.recv().await?;
            let note = |kind: Passed, bytes: usize| {
                if let Some(probe) = &probe {
                    probe(kind, bytes);
                }
            };
            let (text, kind) = match item {
                Item::Beat(text) => {
                    if Instant::now() >= hard {
                        return None;
                    }
                    (text, Passed::Beat)
                }
                Item::Data(text) => {
                    let verdict = if Instant::now() >= hard {
                        Err(Failure::outcome_unknown())
                    } else {
                        boundary.admit().await
                    };
                    match verdict {
                        // Recheck deadline after awaiting boundary admission.
                        Ok(()) if Instant::now() < hard => (text, Passed::Data),
                        other => {
                            let failure = match other {
                                Err(failure) if Instant::now() < hard => failure,
                                _ => Failure::outcome_unknown(),
                            };
                            let event = failure.sse_event();
                            note(Passed::Error, event.len());
                            return Some((Ok(Bytes::from(event)), (None, permit)));
                        }
                    }
                }
            };
            note(kind, text.len());
            Some((Ok(Bytes::from(text)), (Some(rx), permit)))
        }
    })
}

pub(super) fn response(
    mut ctl: Ctl,
    run: ApiRun,
    identity: Identity,
    include_usage: bool,
    permit: OwnedSemaphorePermit,
) -> Response {
    // Delegate release checks to `gated` at chunk handoff rather than inside the producer.
    ctl.at_handoff = true;
    let ctl = Arc::new(ctl);
    let (tx, rx) = mpsc::channel::<Item>(BUFFER);
    let hard = ctl.hard;
    let every = ctl.gateway.keep_alive;
    let probe = ctl.gateway.probe.clone();
    let boundary: Arc<dyn Boundary> = Arc::new(Gate {
        ctl: ctl.clone(),
        basis: ApiRun {
            state: ApiState::Active,
            ..run.clone()
        },
    });
    tokio::spawn(async move {
        let mut sink = Sink {
            tx,
            last: Instant::now(),
            hard,
            every,
        };
        write(&ctl, run, &identity, include_usage, &mut sink).await;
    });
    let body = Body::from_stream(gated(rx, hard, boundary, probe, Some(permit)));
    let mut response = Response::new(body);
    *response.status_mut() = StatusCode::OK;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

async fn write(ctl: &Ctl, run: ApiRun, identity: &Identity, include_usage: bool, sink: &mut Sink) {
    if !sink
        .data(render::first_chunk(identity, include_usage))
        .await
    {
        return;
    }
    match ctl.resolve(run, Some(sink)).await {
        Resolved::Answer(answer) => {
            let (text, finish, usage) = answer.parts();
            for chunk in render::delta_chunks(identity, &text, include_usage) {
                if !sink.data(chunk).await {
                    return;
                }
            }
            // Emit finish, optional measured usage, and `[DONE]` together under one boundary check.
            let mut tail = render::finish_chunk(identity, finish, include_usage);
            if include_usage && let Some(usage) = usage {
                tail.push_str(&render::usage_chunk(identity, usage));
            }
            tail.push_str(render::DONE);
            sink.data(tail).await;
        }
        Resolved::Failure(failure) => {
            sink.data(failure.sse_event()).await;
        }
        Resolved::Gone => {}
    }
}

#[cfg(test)]
mod tests;
