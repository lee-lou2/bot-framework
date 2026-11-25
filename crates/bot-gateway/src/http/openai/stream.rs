//! `stream: true` 응답. 인증·검증·접수를 마친 뒤에야 200 `text/event-stream`을 연다. 헤더를 보낸 뒤의 오류는 `data: {"error":…}` 한 줄 뒤 스트림을 닫는다
//! (성공 종료 표지나 `[DONE]` 없이). 기다리는 동안 `: keep-alive`를 15초 안에 한 번 이상 쓰고, 상대가 끊으면 기다림만 멈춘다 — 접수한 작업은 그대로 두며
//! 같은 키로 다시 조회하면 새 실행 없이 같은 작업에 붙는다.
//!
//! **권한·기한은 조각을 HTTP 본문으로 «넘기는 순간»에 확인한다.** 생산자(`write`)는 조각을 작은 큐에 넣기만 하고, 본문 쪽(`gated`)이 큐에서 꺼내 hyper에 넘기기 직전에
//! 키·멤버·모델 바인딩·활성 집합과 총 기한을 다시 확인한다. 그래서 상대가 읽기를 멈췄다 풀어도, 큐에서 자리를 오래 기다렸어도, 큐에 남아 있던 조각도 모두
//! 꺼내는 순간의 확인을 거친다. 어긋나면 아직 넘기지 않은 조각은 버리고 오류 사건 하나로 닫는다(이미 hyper가 받은 바이트는 회수하지 못한다).
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

/// 쓰기 없이 기다릴 수 있는 최대 시간. 계약은 15초이고, 여유를 둔다.
pub const KEEP_ALIVE_EVERY: Duration = Duration::from_secs(10);
/// 상대가 읽는 속도보다 앞서가지 않도록 하는 작은 큐(조각 수)
const BUFFER: usize = 2;

/// 큐에 담는 조각
pub(super) enum Item {
    /// run에서 나온 것이 없는 keep-alive 주석
    Beat(String),
    /// run에서 나온 모든 것(역할 청크·본문·종료·오류 사건): 넘기는 순간 확인을 거친다
    Data(String),
}

/// 넘기는 경계를 지난 것을 알리는 관찰 지점(시험용)
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Passed {
    /// keep-alive 주석
    Beat,
    /// 본문 조각
    Data,
    /// 확인에 실패해 대신 나간 오류 사건
    Error,
}

pub type Probe = Arc<dyn Fn(Passed, usize) + Send + Sync>;

/// 조각을 넘겨도 되는지(권한·기한) 정하는 경계 확인
pub(super) trait Boundary: Send + Sync + 'static {
    fn admit(&self) -> BoxFuture<'_, Result<(), Failure>>;
}

/// 운영 경계: 내보내기 직전 확인(`Ctl::release`)을 접수한 작업의 고정 사실로 한다.
struct Gate {
    ctl: Arc<Ctl>,
    /// 접수한 작업의 고정 사실(상태 글은 필요 없다)
    basis: ApiRun,
}

impl Boundary for Gate {
    fn admit(&self) -> BoxFuture<'_, Result<(), Failure>> {
        Box::pin(self.ctl.release(&self.basis))
    }
}

/// 생산자가 큐에 쓰는 쪽. 쓰기도 총 기한을 따른다(읽지 않는 상대는 기한에 끊는다).
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

    /// 마지막 쓰기 뒤 오래됐으면 `: keep-alive`를 쓴다. 쓰지 못하면(끊김·정체) false.
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

    /// 다음 keep-alive까지 남은 시간(이미 지났으면 0)
    pub(super) fn until_keep_alive(&self) -> Duration {
        self.every.saturating_sub(self.last.elapsed())
    }

    /// 상대가 끊어 더 읽지 않는다
    pub(super) async fn gone(&self) {
        self.tx.closed().await;
    }
}

/// 큐의 조각을 하나씩 꺼내 «넘기는 순간»에 경계를 확인하고 넘기는 본문 스트림.
/// - `Beat`는 총 기한 전에만 넘긴다(기한 뒤에는 주석도 쓰지 않고 닫는다).
/// - `Data`는 기한 안이고 경계 확인이 통과했을 때만 넘긴다. 기한을 넘겼거나 확인이 실패하면 그 조각과 큐에 남은 조각은 버리고 오류 사건 하나로 닫는다
///   (기한이면 `request_outcome_unknown`, 아니면 확인이 알린 이유).
///
/// 동시 연결 자리(`permit`)는 이 스트림이 쥔다: 본문이 끝나거나(에러 사건으로 닫힌 경우 포함) 버려질 때까지 놓지 않는다. 생산자가 일찍 끝나도 본문이 아직 재확인을
/// 기다리는 동안은 연결이 살아 있으므로 자리를 돌려주지 않는다.
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
                        // 확인을 마친 바로 뒤에도 기한이 지났으면 넘기지 않는다
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

/// 200 + SSE 본문. 쓰는 일은 별도 작업이 맡고, 동시 연결 자리는 본문 스트림이 쥔다(본문이 끝나거나 버려질 때까지).
pub(super) fn response(
    mut ctl: Ctl,
    run: ApiRun,
    identity: Identity,
    include_usage: bool,
    permit: OwnedSemaphorePermit,
) -> Response {
    // 이 응답의 권한·기한 확인은 조각을 넘기는 순간에 한다(생산자는 따로 확인하지 않는다)
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

/// 생산자: 무엇을 보낼지 정해 큐에 넣는다. 권한·기한은 여기서 보지 않는다(넘기는 순간 `gated`가 본다).
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
            // 종료·(측정했을 때만) 사용량·끝 표지는 한 조각이다: 본문을 다 넘긴 뒤 같은 확인 한 번으로 나간다
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
