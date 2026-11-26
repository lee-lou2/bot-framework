//! 회신 발송. outbox에서 자기 책임 범위의 회신을 `claim`하고, 현재 판본으로 바인딩·주체·대상을 확인한 뒤 `begin`이 Ok일 때만
//! **한 번** 보내고 결과를 `settle`한다. 숨은 재전송은 없다 — 결과가 불명이면 `Held`로 두고, 미실행이 확정된 일시 실패만
//! `NotSent`로 정산해 저장소가 백오프 뒤 새 세대로 다시 잡게 한다. CLI·HTTP는 상태 조회로 받으므로 여기 오지 않는다.
use crate::{inbound::kind_name, ingress::Ingress, observe, reply::ReplyBody};
use bot_core::{
    intake::{DeliveryOutcome, DeliveryScope, NotSentReason, Outbox, Outgoing},
    port::{Error, Gateway, Reply},
    profile::Snapshot,
    trace::now_nano,
};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const BATCH: u32 = 8;
const IDLE: Duration = Duration::from_millis(500);
const BACKOFF: Duration = Duration::from_secs(2);
/// 발송 하나가 이보다 오래 걸리면 결과를 불명(Held)으로 정산한다.
const SEND_TIMEOUT: Duration = Duration::from_secs(150);

/// 발송 시도 결과. `Held`는 요청을 쓴 뒤의 불명이고 `NotSent`는 미실행이 확정된 실패다.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Delivery {
    Sent {
        receipt: String,
    },
    NotSent {
        reason: NotSentReason,
        retry_after: Option<Duration>,
    },
    Rejected,
    Held,
}

/// push 발송자 하나. 선언한 범위의 회신을 맡는다.
pub trait Sender: Send + Sync {
    fn scope(&self) -> DeliveryScope;
    /// 현재 판본에서 같은 어댑터가 그 바인딩과 목적지로 발송할 수 있는가. 아니면 거절로 정산한다.
    fn allows(&self, snapshot: &Snapshot, item: &Outgoing) -> bool;
    fn send<'a>(
        &'a self,
        snapshot: &'a Snapshot,
        item: &'a Outgoing,
        body: &'a ReplyBody,
    ) -> Pin<Box<dyn Future<Output = Delivery> + Send + 'a>>;
}

pub struct Deliverer {
    outbox: Arc<dyn Outbox>,
    ingress: Arc<Ingress>,
    sender: Arc<dyn Sender>,
    wake: Notify,
}

impl Deliverer {
    pub fn new(outbox: Arc<dyn Outbox>, ingress: Arc<Ingress>, sender: Arc<dyn Sender>) -> Self {
        Self {
            outbox,
            ingress,
            sender,
            wake: Notify::new(),
        }
    }

    /// 한 번 훑는다. 처리한 항목 수를 돌려준다.
    pub async fn tick(&self) -> Result<usize, Error> {
        let items = self.outbox.claim(&self.sender.scope(), BATCH).await?;
        let count = items.len();
        for item in items {
            self.deliver_one(item).await;
        }
        Ok(count)
    }

    /// stop까지 돈다. 처리할 것이 남아 있으면 바로 다시 훑고, 없으면 잠깐 쉬거나 `deliver` 신호를 기다린다.
    pub async fn run(&self, stop: CancellationToken) {
        loop {
            let pause = match self.tick().await {
                Ok(n) if n >= BATCH as usize => Duration::ZERO,
                Ok(_) => IDLE,
                Err(error) => {
                    tracing::warn!(?error, "회신 발송 예약을 잡지 못했어요");
                    BACKOFF
                }
            };
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(pause) => {}
            }
        }
    }

    async fn deliver_one(&self, item: Outgoing) {
        let started = now_nano();
        let key = item.key.clone();
        let binding = item.binding.0.clone();
        let conversation = kind_name(&item.destination.kind);
        // 발송 세대는 worker 시도가 아니다 — 가짜 Attempt 없이 (프로필, run)과 속성으로 잇는다
        let trace = |outcome: &'static str, error: Option<&'static str>| {
            observe::record_delivery(
                &**self.ingress.tracer(),
                &observe::Delivered {
                    profile: &key.profile,
                    run: &key.run,
                    binding: &binding,
                    conversation,
                    outcome,
                    error,
                    started,
                },
            );
        };
        let snapshot = match self.ingress.routes() {
            Ok(routes) => routes.snapshot(&key.profile).cloned(),
            Err(_) => None,
        };
        let Some(snapshot) = snapshot else {
            // 활성 프로필이 아니면 보내지 않는다. 리스가 끝나면 저장소가 다시 잡게 둔다.
            tracing::warn!(profile = %key.profile.0, run = %key.run.0, "활성 프로필이 아니라 회신을 보내지 않아요");
            return trace("skipped", None);
        };
        let body = ReplyBody::parse(&item.body);
        let allowed = snapshot.members.contains_key(&item.principal)
            && snapshot.bindings.contains_key(&item.binding)
            && self.sender.allows(&snapshot, &item);
        let revision = snapshot.revision.clone();
        let body = match body {
            Ok(body) if allowed => body,
            invalid => {
                if let Err(reason) = invalid {
                    tracing::warn!(profile = %key.profile.0, run = %key.run.0, reason, "회신 본문이 올바르지 않아요");
                }
                // 보내지 않는다. begin으로 점유를 기록하고 거절로 정산해 같은 항목이 다시 잡히지 않게 한다.
                if self.outbox.begin(key.clone(), revision).await.is_ok()
                    && let Err(error) = self
                        .outbox
                        .settle(key.clone(), DeliveryOutcome::Rejected)
                        .await
                {
                    tracing::error!(?error, "거절 정산을 기록하지 못했어요");
                }
                return trace("rejected", Some("rejected"));
            }
        };
        // begin이 Ok일 때만 전송자다. 실패하거나 응답을 잃으면 보내지 않는다.
        if self.outbox.begin(key.clone(), revision).await.is_err() {
            return trace("skipped", None);
        }
        let delivery =
            tokio::time::timeout(SEND_TIMEOUT, self.sender.send(&snapshot, &item, &body))
                .await
                .unwrap_or(Delivery::Held);
        let (outcome, label, error) = match delivery {
            Delivery::Sent { receipt } => {
                (DeliveryOutcome::Confirmed { receipt }, "confirmed", None)
            }
            Delivery::NotSent {
                reason,
                retry_after,
            } => (
                DeliveryOutcome::NotSent {
                    reason,
                    retry_after,
                },
                "not_sent",
                Some("not_sent"),
            ),
            Delivery::Rejected => (DeliveryOutcome::Rejected, "rejected", Some("rejected")),
            Delivery::Held => (DeliveryOutcome::Held, "held", Some("held")),
        };
        if let Err(error) = self.outbox.settle(key.clone(), outcome).await {
            // 결과를 기록하지 못해도 다시 보내지 않는다(저장소는 sending을 불명으로 다룬다)
            tracing::error!(?error, "회신 정산을 기록하지 못했어요");
        }
        trace(label, error);
    }
}

/// 런타임이 회신을 만들었다는 신호. 다른 프로세스에서는 닿지 않으므로 폴링이 기본이고 이것은 지연을 줄이는 용도다.
impl Gateway for Deliverer {
    fn deliver(
        &self,
        _run: bot_core::id::RunId,
        _profile: bot_core::id::ProfileId,
    ) -> Reply<'_, ()> {
        self.wake.notify_one();
        Box::pin(std::future::ready(Ok(())))
    }
}
