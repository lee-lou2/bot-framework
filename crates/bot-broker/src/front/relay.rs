//! Model request relay handling streaming selection, ownership verification, and turn publication before response release.

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

/// Runs `work` under ownership monitoring and `limit`; preserves `paused` state for the pre-release `settle()`.
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
        // Publish completed tool calls before releasing a 2xx response to the CLI.
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
                // Report unusable upstream responses as retryable `Unavailable`, not client errors.
                Ok(Err(error @ (Error::Stale | Error::Denied))) | Err(error) => Err(error),
                Ok(Err(_)) => Err(Error::Unavailable),
            }
        }
        other => other,
    };
    // Re-confirm ownership immediately before writing any response (including upstream errors).
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

async fn relay(
    client: &mut TcpStream,
    shared: &Shared,
    own: &mut Ownership<'_>,
    req: ModelRequest,
) {
    let cfg = &shared.cfg;
    let start = shared.parts.model.forward_stream(req);
    tokio::pin!(start);
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
        if let Err(error) = own.settle().await {
            return write_error(client, cfg, &error).await;
        }
        return write_response(client, status, "application/json", &body, cfg.write_timeout).await;
    }
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
            // Emit a fixed error before writing any unobservable raw event bytes.
            let _ = http::write_chunk(&mut writer, &error_event("unavailable"), cfg.write_timeout)
                .await;
            break;
        }
        if observer.stopped() {
            // Publish the completed turn's tool list and re-confirm ownership before emitting `message_stop`.
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

async fn publish_body(shared: &Shared, attempt: &Attempt, body: &str) -> Result<(), Error> {
    match observe_body(body)? {
        Some(turn) => shared.parts.turns.completed(attempt.clone(), turn).await,
        None => Ok(()),
    }
}
