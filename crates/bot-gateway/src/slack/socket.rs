//! Slack Socket Mode 연결 루프. 연결·재접속·ack만 한다. 받은 것을 처리할지는 호출한 쪽(`Handler`)이 정한다.
//!
//! - hello가 기대한 앱임을 확인하기 전에는 아무것도 처리하지도 ack하지도 않는다(앱이 다르면 멈춘다).
//! - 글(멘션·DM)은 처리기가 «안전하게 접수했다(true)»고 답한 뒤에만 ack한다. 아니면 ack하지 않아 Slack이 다시 보낸다.
//! - 버튼도 처리기가 «판정했다(true)»고 답한 뒤에만 ack한다(일시 실패는 ack하지 않는다). 같은 버튼이 다시 와도 원장이 결정을 한 번만 반영한다.
//! - 처리는 이벤트마다 따로(동시 상한 있음)라 느린 접수가 핑이나 다른 이벤트를 막지 않는다.
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

/// 인증된 설치 하나: (프로필, 워크스페이스 별칭)의 자격으로 연 소켓이 닿는 범위.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Installation {
    pub profile: ProfileId,
    pub alias: String,
    pub team: String,
    /// 있으면 소켓 hello가 이 앱이 아닐 때 멈춘다.
    pub app_id: Option<String>,
    /// 봇 사용자 ID(`auth.test`). 글에서 봇 멘션을 떼는 데 쓴다.
    pub bot_user: String,
}

/// 받은 프레임을 처리해 «ack해도 되는가»를 돌려준다.
pub type Handler = Arc<dyn Fn(Frame) -> Pin<Box<dyn Future<Output = bool> + Send>> + Send + Sync>;

#[derive(Debug)]
pub struct Fatal(pub String);

/// stop까지 연결을 유지한다. 앱 불일치처럼 다시 시도해도 소용없는 오류만 `Err`로 끝낸다.
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
                tracing::warn!(profile = %install.profile.0, workspace = %install.alias, reason, "slack 소켓 연결에 실패했어요");
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
        .ok_or(Failure::Retry("앱 토큰을 읽지 못했어요"))?;
    let url = api
        .open_socket(&app_token)
        .await
        .ok_or(Failure::Retry("소켓 주소를 받지 못했어요"))?;
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .map_err(|_| Failure::Retry("웹소켓에 연결하지 못했어요"))?;
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
                                    "프로필 {}의 워크스페이스 {}: 설정한 Slack 앱과 연결된 앱이 달라요 — 서로 다른 앱의 토큰이 섞였어요",
                                    install.profile.0, install.alias
                                ))));
                            }
                            (Some(_), None) => {
                                let _ = ws.close(None).await;
                                return Err(Failure::Fatal(Fatal(format!(
                                    "프로필 {}의 워크스페이스 {}: 연결된 Slack 앱을 확인할 수 없어요",
                                    install.profile.0, install.alias
                                ))));
                            }
                            _ => {}
                        }
                        saw_hello = true;
                    }
                    Frame::Disconnect(reason) => {
                        tracing::info!(workspace = %install.alias, reason, "slack이 연결을 닫아 달라고 했어요 — 다시 접속해요");
                        if let Some(id) = ack {
                            let _ = ws.send(ack_frame(&id)).await;
                        }
                        let _ = ws.close(None).await;
                        break;
                    }
                    // hello로 앱을 확인하기 전에는 처리하지도 ack하지도 않는다(Slack이 확인된 연결로 다시 보낸다)
                    _ if !saw_hello => tracing::warn!("hello 전에 프레임이 왔어요 — 처리하지 않아요"),
                    Frame::Message(_) | Frame::Post(_) | Frame::Reaction(_) | Frame::Button(_) => {
                        let (handler, ack_tx, limit) = (handler.clone(), ack_tx.clone(), limit.clone());
                        let Ok(permit) = limit.try_acquire_owned() else {
                            tracing::warn!("처리 중인 이벤트가 너무 많아요 — ack하지 않아 다시 받아요");
                            continue;
                        };
                        tokio::spawn(async move {
                            // 접수·판정이 확정됐을 때만 ack한다. 일시 실패는 ack하지 않아 Slack이 다시 보낸다.
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
