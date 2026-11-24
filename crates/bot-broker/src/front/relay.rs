//! 모델 요청 중계: 본문의 `stream` 값으로 스트리밍 여부를 정하고, 소유권을 확인하며, 완료한 응답의 도구 목록을 알린 뒤에만 내보낸다.

use super::{
    FrontConfig, Session, Shared, error_body,
    http::{self, Request, write_response},
    http_error,
    observe::{Observer, observe_body},
    owner::{Change, Ownership},
};
use crate::model::{ModelRequest, error_event};
use bot_core::{command::JsonPayload, id::Attempt, port::Error};
use serde_json::Value;
use std::{future::Future, time::Duration};
use tokio::{io::AsyncWriteExt, net::TcpStream, sync::watch, time::timeout};

/// 모델 요청 하나. 본문의 `stream` 값으로 스트리밍 여부를 정한다.
pub(super) async fn model(
    client: &mut TcpStream,
    shared: &Shared,
    (session, gone): (Session, watch::Receiver<bool>),
    request: Request,
) {
    let cfg = &shared.cfg;
    let streaming = matches!(
        std::str::from_utf8(&request.body).ok().and_then(crate::json::parse),
        Some(Value::Object(map)) if map.get("stream") == Some(&Value::Bool(true))
    );
    let body = match String::from_utf8(request.body.clone()) {
        Ok(text) => JsonPayload(text),
        Err(_) => {
            let body = error_body("invalid_request_error", "invalid");
            return write_response(
                client,
                400,
                "application/json",
                body.as_bytes(),
                cfg.write_timeout,
            )
            .await;
        }
    };
    let req = ModelRequest {
        attempt: session.attempt.clone(),
        alias: session.alias.clone(),
        body,
        headers: request.headers,
    };
    let mut own = Ownership::new(shared, &session, gone);
    if streaming {
        relay(client, shared, &mut own, req).await;
    } else {
        plain(client, shared, &mut own, req).await;
    }
}

async fn write_error(client: &mut TcpStream, cfg: &FrontConfig, error: &Error) {
    let (code, kind, message) = http_error(error);
    let body = error_body(kind, message);
    write_response(
        client,
        code,
        "application/json",
        body.as_bytes(),
        cfg.write_timeout,
    )
    .await;
}

/// `work`를 기다리는 동안에도 소유권을 보고 시간 한도(`limit`)를 지킨다. 잃으면 `Stale`, 확인 불가가 기한을 넘기거나 한도를
/// 넘기면 `Unavailable`이다. 확인 불가 상태(`paused`)는 이어 간다(내보내기 직전의 `settle`이 풀릴 때까지 기다린다).
async fn guarded<T>(
    own: &mut Ownership<'_>,
    paused: &mut bool,
    limit: Duration,
    work: impl Future<Output = T>,
) -> Result<T, Error> {
    let work = timeout(limit, work);
    tokio::pin!(work);
    loop {
        tokio::select! {
            biased;
            change = own.changed() => match change {
                Change::Confirmed => *paused = false,
                Change::Unconfirmed => *paused = true,
                Change::Lost => return Err(Error::Stale),
                Change::Expired => return Err(Error::Unavailable),
            },
            out = &mut work => return out.map_err(|_| Error::Unavailable),
        }
    }
}

/// 스트리밍이 아닌 요청. 응답을 받아도 소유권을 확인할 수 없으면 기한까지 내보내지 않고, 잃었으면 상류 요청을 내려놓는다.
async fn plain(
    client: &mut TcpStream,
    shared: &Shared,
    own: &mut Ownership<'_>,
    req: ModelRequest,
) {
    let cfg = &shared.cfg;
    let forward = shared.parts.model.forward(req);
    tokio::pin!(forward);
    let (mut paused, mut done) = (false, None);
    let outcome = loop {
        tokio::select! {
            biased;
            change = own.changed() => match change {
                Change::Confirmed => paused = false,
                Change::Unconfirmed => paused = true,
                Change::Lost => break Err(Error::Stale),
                Change::Expired => break Err(Error::Unavailable),
            },
            reply = &mut forward, if done.is_none() => done = Some(reply),
        }
        if !paused && let Some(reply) = done.take() {
            break reply;
        }
    };
    let outcome = match outcome {
        // 완료한 응답의 도구 목록을 알리는 데 성공한 뒤에야 응답을 내보낸다. 알리는 동안에도 소유권과 시간 한도를 본다.
        Ok(reply) if (200..300).contains(&reply.status) => {
            let attempt = own.attempt().clone();
            let told = guarded(
                own,
                &mut paused,
                cfg.publish_timeout,
                publish_body(shared, &attempt, &reply.body.0),
            )
            .await;
            match told {
                Ok(Ok(())) => Ok(reply),
                // 쓸 수 없는 응답은 클라이언트의 잘못된 요청이 아니라 상류 문제다(CLI가 다시 시도하는 종류로 알린다).
                Ok(Err(error @ (Error::Stale | Error::Denied))) | Err(error) => Err(error),
                Ok(Err(_)) => Err(Error::Unavailable),
            }
        }
        other => other,
    };
    // 내보내기 직전에 소유권을 새로 확인한다(상류의 오류 응답도 확인 뒤에 내보낸다).
    let outcome = match outcome {
        Ok(reply) => own.settle().await.map(|()| reply),
        other => other,
    };
    match outcome {
        Ok(reply) => {
            write_response(
                client,
                reply.status,
                "application/json",
                reply.body.0.as_bytes(),
                cfg.write_timeout,
            )
            .await
        }
        Err(error) => write_error(client, cfg, &error).await,
    }
}

/// 스트리밍 요청. 소유권을 확인할 수 없는 동안 출력을 멈추고(상류도 읽지 않는다), 기한을 넘기면 닫는다.
/// 잃었으면(토큰 회수·인계·종료) 고정 오류 이벤트 하나로 끝내고 상류를 닫는다.
async fn relay(
    client: &mut TcpStream,
    shared: &Shared,
    own: &mut Ownership<'_>,
    req: ModelRequest,
) {
    let cfg = &shared.cfg;
    let start = shared.parts.model.forward_stream(req);
    tokio::pin!(start);
    // 상류 응답을 기다리는 동안에도 소유권을 놓치지 않는다. 확인 불가 상태는 내보내기 직전의 `settle`이 풀릴 때까지 기다리게 한다.
    let started = loop {
        tokio::select! {
            biased;
            change = own.changed() => match change {
                Change::Lost => break Err(Error::Stale),
                Change::Expired => break Err(Error::Unavailable),
                Change::Confirmed | Change::Unconfirmed => {}
            },
            started = &mut start => break started,
        }
    };
    let mut stream = match started {
        Ok(stream) => stream,
        Err(error) => return write_error(client, cfg, &error).await,
    };
    if !(200..300).contains(&stream.status()) {
        let status = stream.status();
        let body = stream.next().await.unwrap_or_default();
        // 상류의 오류 응답도 소유권을 확인한 뒤에 내보낸다.
        if let Err(error) = own.settle().await {
            return write_error(client, cfg, &error).await;
        }
        return write_response(client, status, "application/json", &body, cfg.write_timeout).await;
    }
    // 소유권을 새로 확인하기 전에는 머리도 내보내지 않는다(확인할 수 없으면 기한까지 기다리고, 넘기면 고정 오류로 닫는다).
    if let Err(error) = own.settle().await {
        return write_error(client, cfg, &error).await;
    }
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream; charset=utf-8\r\ncache-control: no-cache\r\nconnection: close\r\ntransfer-encoding: chunked\r\n\r\n";
    let (mut reader, mut writer) = client.split();
    if !http::write_all(&mut writer, head.as_bytes(), cfg.write_timeout).await {
        return;
    }
    let mut paused = false;
    let mut observer = Observer::default();
    loop {
        // 다음 이벤트를 기다리는 동안: 클라이언트가 떠나면 곧바로 스트림을 내려놓아 상류를 닫고 자리를 돌려준다.
        let next = stream.next();
        tokio::pin!(next);
        let event = loop {
            tokio::select! {
                biased;
                change = own.changed() => match change {
                    Change::Confirmed => paused = false,
                    Change::Unconfirmed => paused = true,
                    Change::Lost => {
                        let _ = http::write_chunk(&mut writer, &error_event("stale"), cfg.write_timeout).await;
                        break None;
                    }
                    Change::Expired => {
                        let _ = http::write_chunk(&mut writer, &error_event("unavailable"), cfg.write_timeout).await;
                        break None;
                    }
                },
                () = http::closed(&mut reader) => return,
                event = &mut next, if !paused => break event,
            }
        };
        let Some(event) = event else { break };
        observer.feed(&event);
        if observer.broken() {
            // 관측할 수 없는 이벤트는 원문을 쓰기 전에 고정 오류로 닫는다(종결 이벤트처럼 보여도 내보내지 않는다).
            let _ = http::write_chunk(&mut writer, &error_event("unavailable"), cfg.write_timeout)
                .await;
            break;
        }
        if observer.stopped() {
            // 완료한 응답의 도구 목록을 알리는 데 성공한 뒤에야 종결 이벤트를 내보낸다. 못 알리면 오류로 끝낸다.
            let told = match observer.finish() {
                Ok(Some(turn)) => {
                    let attempt = own.attempt().clone();
                    let work = shared.parts.turns.completed(attempt, turn);
                    match guarded(own, &mut paused, cfg.publish_timeout, work).await {
                        Ok(told) => told,
                        Err(error) => Err(error),
                    }
                }
                Ok(None) => Ok(()),
                Err(error) => Err(error),
            };
            // 알린 뒤 종결 이벤트를 내보내기 직전에 소유권을 새로 확인한다.
            let told = match told {
                Ok(()) => own.settle().await,
                Err(error) => Err(error),
            };
            if let Err(error) = told {
                let class = if matches!(error, Error::Stale | Error::Denied) {
                    "stale"
                } else {
                    "unavailable"
                };
                let _ =
                    http::write_chunk(&mut writer, &error_event(class), cfg.write_timeout).await;
                break;
            }
        }
        if !http::write_chunk(&mut writer, &event, cfg.write_timeout).await {
            return;
        }
    }
    let _ = timeout(cfg.write_timeout, writer.write_all(b"0\r\n\r\n")).await;
    let _ = writer.shutdown().await;
}

/// 스트리밍이 아닌 응답 본문의 도구 목록을 알린다.
async fn publish_body(shared: &Shared, attempt: &Attempt, body: &str) -> Result<(), Error> {
    match observe_body(body)? {
        Some(turn) => shared.parts.turns.completed(attempt.clone(), turn).await,
        None => Ok(()),
    }
}
