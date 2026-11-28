//! Owner-DM approval card delivery loop and receipt verification for Telegram group-origin approvals.
use super::{
    Installation,
    send::{CardPost, TelegramSender},
};
use crate::{identity, ingress::Ingress, reply::parse_card};
use bot_core::{
    cards::{CardJob, CardKey, CardOutbox, CardOutcome, CardReceipt},
    id::{BindingId, PrincipalId, Revision},
    intake::PushTransportId,
    port::Error,
    profile::{Adapter, Snapshot},
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio_util::sync::CancellationToken;

const BATCH: u32 = 8;
const IDLE: Duration = Duration::from_millis(500);
const BACKOFF: Duration = Duration::from_secs(2);
const SEND_TIMEOUT: Duration = Duration::from_secs(60);

fn group_bot<'a>(snapshot: &'a Snapshot, binding: &BindingId) -> Option<&'a str> {
    match snapshot.bindings.get(binding).map(|b| &b.source) {
        Some(Adapter::TelegramGroups { bot }) => Some(bot.as_str()),
        _ => None,
    }
}

fn telegram_user_id(principal: &PrincipalId) -> Option<u64> {
    principal
        .0
        .strip_prefix("telegram:")
        .filter(|digits| !digits.is_empty() && !digits.starts_with('0'))
        .and_then(|digits| digits.parse::<u64>().ok())
        .filter(|id| *id > 0)
}

/// Verifies that a DM callback click matches the posted owner-DM approval card receipt.
pub fn is_the_card(
    receipt: &CardReceipt,
    install: &Installation,
    snapshot: &Snapshot,
    chat_id: u64,
    message_id: u64,
    user_id: u64,
) -> bool {
    !receipt.ts.is_empty()
        && receipt.profile == install.profile
        && receipt.team == install.bot_id.to_string()
        && receipt.dm == chat_id.to_string()
        && receipt.ts == message_id.to_string()
        && identity::telegram(user_id).as_ref() == Some(&receipt.owner)
        && group_bot(snapshot, &receipt.binding) == Some(install.alias.as_str())
}

struct Unsettled {
    key: CardKey,
    outcome: CardOutcome,
}

enum Verdict {
    Post(String, u64, Revision),
    Close,
    Leave,
}

pub struct TelegramCardDeliverer {
    jobs: Arc<dyn CardOutbox>,
    ingress: Arc<Ingress>,
    sender: TelegramSender,
    unsettled: Mutex<Vec<Unsettled>>,
}

impl TelegramCardDeliverer {
    pub fn new(jobs: Arc<dyn CardOutbox>, ingress: Arc<Ingress>, sender: TelegramSender) -> Self {
        Self {
            jobs,
            ingress,
            sender,
            unsettled: Mutex::new(Vec::new()),
        }
    }

    pub async fn tick(&self) -> Result<usize, Error> {
        self.resettle().await;
        let jobs = self
            .jobs
            .claim_for(&PushTransportId::TELEGRAM, BATCH)
            .await?;
        let count = jobs.len();
        for job in jobs {
            self.deliver_one(job).await;
        }
        Ok(count)
    }

    pub async fn run(&self, stop: CancellationToken) {
        loop {
            let pause = match self.tick().await {
                Ok(n) if n >= BATCH as usize => Duration::ZERO,
                Ok(_) => IDLE,
                Err(error) => {
                    tracing::warn!(?error, "failed to claim Telegram approval card jobs");
                    BACKOFF
                }
            };
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(pause) => {}
            }
        }
    }

    async fn judge(&self, job: &CardJob) -> Verdict {
        let Ok(routes) = self.ingress.routes() else {
            return Verdict::Leave;
        };
        let Some(snapshot) = routes.snapshot(&job.profile) else {
            return Verdict::Leave;
        };
        let Some(bot) = group_bot(snapshot, &job.binding) else {
            return Verdict::Close;
        };
        if !snapshot.members.contains_key(&job.owner) {
            return Verdict::Close;
        }
        let Some(user_id) = telegram_user_id(&job.owner) else {
            return Verdict::Close;
        };
        let allowed = self
            .ingress
            .host()
            .telegram
            .bots
            .get(bot)
            .is_some_and(|entry| entry.groups);
        if !allowed {
            return Verdict::Close;
        }
        Verdict::Post(bot.to_owned(), user_id, snapshot.revision.clone())
    }

    async fn deliver_one(&self, job: CardJob) {
        let (bot, user_id, revision) = match self.judge(&job).await {
            Verdict::Leave => return,
            Verdict::Close => {
                self.settle(job.key, CardOutcome::Unavailable).await;
                return;
            }
            Verdict::Post(bot, user_id, revision) => (bot, user_id, revision),
        };
        let card = serde_json::from_str(&job.card.0)
            .ok()
            .and_then(|value| parse_card(&value).ok())
            .filter(|card| card.nonce == job.key.nonce.0);
        let Some(card) = card else {
            self.settle(job.key, CardOutcome::Unavailable).await;
            return;
        };
        if self.jobs.begin(job.key.clone(), revision).await.is_err() {
            return;
        }
        let posted = tokio::time::timeout(
            SEND_TIMEOUT,
            self.sender
                .post_owner_card(&job.profile, &bot, user_id, &card),
        )
        .await
        .unwrap_or(CardPost::Held);
        self.settle(job.key, outcome_of(posted)).await;
    }

    async fn settle(&self, key: CardKey, outcome: CardOutcome) {
        self.unsettled
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Unsettled { key, outcome });
        self.resettle().await;
    }

    async fn resettle(&self) {
        let waiting =
            std::mem::take(&mut *self.unsettled.lock().unwrap_or_else(|e| e.into_inner()));
        for item in waiting {
            match self
                .jobs
                .settle(item.key.clone(), item.outcome.clone())
                .await
            {
                Ok(()) => {}
                Err(Error::Unavailable) => {
                    self.unsettled
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(item);
                }
                Err(error) => {
                    tracing::error!(?error, "Telegram approval card settlement rejected");
                }
            }
        }
    }
}

fn outcome_of(posted: CardPost) -> CardOutcome {
    match posted {
        CardPost::Posted { bot_id, dm, msg_id } => CardOutcome::Posted {
            team: bot_id,
            dm,
            ts: msg_id,
        },
        CardPost::NotSent {
            reason,
            retry_after,
        } => CardOutcome::NotSent {
            reason,
            retry_after,
        },
        CardPost::Unavailable => CardOutcome::Unavailable,
        CardPost::Held => CardOutcome::Held,
    }
}
