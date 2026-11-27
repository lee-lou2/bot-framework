//! Slack Socket Mode connection loop handling reconnects, `hello` app verification, and bounded concurrent acks.
//!
//! Never processes or acknowledges frames until `hello` confirms the expected `app_id`, and acknowledges events
//! only after the handler reports a settled outcome (`true`).
use super::{
    api::SlackApi,
    creds::Credentials,
    frame::{Frame, parse_socket, reconnect_delay_ms},
};
use bot_core::id::ProfileId;
use futures_util::{SinkExt, StreamExt};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, mpsc};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

const MAX_HANDLERS: usize = 64;
const PING_EVERY: Duration = Duration::from_secs(30);
const DEAD_AFTER: Duration = Duration::from_secs(90);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Installation {
    pub profile: ProfileId,
    pub alias: String,
    pub team: String,
    pub app_id: Option<String>,
    pub bot_user: String,
}

pub type Handler = Arc<dyn Fn(Frame) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

#[derive(Debug)]
pub struct Fatal(pub String);

/// Maintains the Socket Mode connection until `stop`, returning `Err` only on non-retryable configuration mismatches.
pub async fn run(
    api: SlackApi,
    creds: Arc<Credentials>,
    install: Installation,
    handler: Handler,
    stop: CancellationToken,
) -> Result<(), Fatal> {
    let limit = Arc::new(Semaphore::new(MAX_HANDLERS));
    let mut attempt = 0u32;
    while !stop.is_cancelled() {
        match connect_once(&api, &creds, &install, &handler, &limit, &stop).await {
            Ok(saw_hello) => attempt = if saw_hello { 1 } else { attempt + 1 },
            Err(Failure::Fatal(error)) => return Err(error),
            Err(Failure::Retry(reason)) => {
                attempt += 1;
                tracing::warn!(profile = %install.profile.0, workspace = %install.alias, reason, "Slack socket connection failed");
            }
        }
        if stop.is_cancelled() {
            break;
        }
        let delay = Duration::from_millis(reconnect_delay_ms(attempt));
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = stop.cancelled() => break,
        }
    }
    Ok(())
}

enum Failure {
    Retry(&'static str),
    Fatal(Fatal),
}

fn ack_frame(envelope_id: &str) -> Message {
    Message::Text(
        serde_json::json!({ "envelope_id": envelope_id })
            .to_string()
            .into(),
    )
}

async fn connect_once(
    api: &SlackApi,
    creds: &Credentials,
    install: &Installation,
    handler: &Handler,
    limit: &Arc<Semaphore>,
    stop: &CancellationToken,
) -> Result<bool, Failure> {
    let app_token = creds
        .app_token(&install.profile, &install.alias)
        .await
        .ok_or(Failure::Retry("failed to read app token"))?;
    let url = api
        .open_socket(&app_token)
        .await
        .ok_or(Failure::Retry("failed to obtain socket URL"))?;
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .map_err(|_| Failure::Retry("failed to connect to WebSocket"))?;
    let (ack_tx, mut acks) = mpsc::unbounded_channel::<String>();
    let mut saw_hello = false;
    let mut tick = tokio::time::interval(PING_EVERY);
    tick.tick().await;
    let mut last_rx = Instant::now();
    loop {
        tokio::select! {
            _ = stop.cancelled() => {
                let _ = ws.close(None).await;
                break;
            }
            _ = tick.tick() => {
                if last_rx.elapsed() > DEAD_AFTER || ws.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            Some(id) = acks.recv() => {
                if ws.send(ack_frame(&id)).await.is_err() {
                    break;
                }
            }
            message = ws.next() => {
                let Some(Ok(message)) = message else { break };
                last_rx = Instant::now();
                let text = match message {
                    Message::Text(text) => text.to_string(),
                    Message::Ping(payload) => {
                        let _ = ws.send(Message::Pong(payload)).await;
                        continue;
                    }
                    Message::Close(_) => break,
                    _ => continue,
                };
                let (frame, ack) = parse_socket(&text);
                match frame {
                    Frame::Hello(app) => {
                        match (&install.app_id, app) {
                            (Some(want), Some(got)) if *want != got => {
                                let _ = ws.close(None).await;
                                return Err(Failure::Fatal(Fatal(format!(
                                    "profile {} workspace {}: connected Slack app does not match configured app — tokens from different apps are mixed",
                                    install.profile.0, install.alias
                                ))));
                            }
                            (Some(_), None) => {
                                let _ = ws.close(None).await;
                                return Err(Failure::Fatal(Fatal(format!(
                                    "profile {} workspace {}: could not verify connected Slack app",
                                    install.profile.0, install.alias
                                ))));
                            }
                            _ => {}
                        }
                        saw_hello = true;
                    }
                    Frame::Disconnect(reason) => {
                        tracing::info!(workspace = %install.alias, reason, "Slack requested disconnect — reconnecting");
                        if let Some(id) = ack {
                            let _ = ws.send(ack_frame(&id)).await;
                        }
                        let _ = ws.close(None).await;
                        break;
                    }
                    _ if !saw_hello => tracing::warn!("received frame before hello — ignoring"),
                    Frame::Message(_)
                    | Frame::Post(_)
                    | Frame::Reaction(_)
                    | Frame::Button(_)
                    | Frame::StopRequested(_) => {
                        let (handler, ack_tx, limit) = (handler.clone(), ack_tx.clone(), limit.clone());
                        let Ok(permit) = limit.try_acquire_owned() else {
                            tracing::warn!("too many concurrent events — skipping ack for redelivery");
                            continue;
                        };
                        tokio::spawn(async move {
                            if handler(frame).await
                                && let Some(id) = ack
                            {
                                let _ = ack_tx.send(id);
                            }
                            drop(permit);
                        });
                    }
                    Frame::Ignored | Frame::Invalid => {
                        if let Some(id) = ack {
                            let _ = ws.send(ack_frame(&id)).await;
                        }
                    }
                }
            }
        }
    }
    Ok(saw_hello)
}
