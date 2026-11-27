//! Owner-DM approval card delivery loop and receipt-bound button click verification.
use super::{
    frame::Button,
    send::{CardPost, SlackSender},
    socket::Installation,
};
use crate::{identity, ingress::Ingress, reply::parse_card};
pub use bot_core::cards::{CardJob, CardKey, CardOutbox, CardOutcome, CardReceipt};
use bot_core::{
    api::ApprovalBlock,
    cards::CardOrigin,
    id::{AgentId, BindingId, PrincipalId, Revision},
    port::Error,
    profile::{Adapter, ApprovalRoute, Snapshot},
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

fn channel_workspace<'a>(snapshot: &'a Snapshot, binding: &BindingId) -> Option<&'a str> {
    match snapshot.bindings.get(binding).map(|b| &b.source) {
        Some(Adapter::SlackChannel { workspace, .. }) => Some(workspace),
        _ => None,
    }
}

/// Verifies that a button click matches the posted DM card's receipt (profile, team, DM, ts, owner, and channel binding).
pub fn is_the_card(
    receipt: &CardReceipt,
    install: &Installation,
    snapshot: &Snapshot,
    button: &Button,
) -> bool {
    !receipt.ts.is_empty()
        && receipt.profile == install.profile
        && receipt.team == install.team
        && button.team == install.team
        && button.channel == receipt.dm
        && button.message_ts == receipt.ts
        && identity::slack(&install.alias, &button.user).as_ref() == Some(&receipt.owner)
        && channel_workspace(snapshot, &receipt.binding) == Some(install.alias.as_str())
}

#[derive(Clone)]
enum Settlement {
    Outcome(CardOutcome),
    Block(ApprovalBlock),
}

/// Retries recording an already-decided settlement without re-posting the card.
struct Unsettled {
    key: CardKey,
    settlement: Settlement,
}

enum Verdict {
    Post(String, Revision),
    Close,
    Block(ApprovalBlock),
    Leave,
}

struct Origin {
    endpoint: String,
    model: String,
    agent: AgentId,
    route: ApprovalRoute,
}

fn origin_in(snapshot: &Snapshot, binding: &BindingId, requester: &PrincipalId) -> Option<Origin> {
    let route = snapshot.approval_route(binding, requester)?;
    let found = snapshot.bindings.get(binding)?;
    let Adapter::OpenAi {
        endpoint, model, ..
    } = &found.source
    else {
        return None;
    };
    Some(Origin {
        endpoint: endpoint.clone(),
        model: model.clone(),
        agent: found.agent.clone(),
        route,
    })
}

/// Ensures an HTTP-origin card still matches its pinned endpoint, model, agent, approver, and delivery binding.
/// Distinguishes revoked approvers (`ApproverRevoked`) from any other route/target drift (`RouteChanged`).
fn route_problem(
    pinned: Option<&Snapshot>,
    current: &Snapshot,
    origin: &CardOrigin,
    job: &CardJob,
) -> Option<ApprovalBlock> {
    let facts = |snapshot: &Snapshot| origin_in(snapshot, &origin.binding, &origin.requester);
    let card_route = |facts: &Origin| matches!(&facts.route, ApprovalRoute::Slack { person, binding } if *person == job.owner && *binding == job.binding);
    let Some(then) = pinned.and_then(facts).filter(card_route) else {
        return Some(ApprovalBlock::RouteChanged);
    };
    if let Some(now) = facts(current).filter(card_route) {
        let same =
            (&now.endpoint, &now.model, &now.agent) == (&then.endpoint, &then.model, &then.agent);
        return (!same).then_some(ApprovalBlock::RouteChanged);
    }
    let mapped = match current.bindings.get(&origin.binding).map(|b| &b.source) {
        Some(Adapter::OpenAi { approvers, .. })
            if current.members.contains_key(&origin.requester) =>
        {
            approvers.get(&origin.requester)
        }
        _ => return Some(ApprovalBlock::RouteChanged),
    };
    match mapped {
        Some(route) if current.members.contains_key(route.person()) => {
            Some(ApprovalBlock::RouteChanged)
        }
        _ => Some(ApprovalBlock::ApproverRevoked),
    }
}

pub struct CardDeliverer {
    jobs: Arc<dyn CardOutbox>,
    ingress: Arc<Ingress>,
    sender: SlackSender,
    unsettled: Mutex<Vec<Unsettled>>,
}

impl CardDeliverer {
    pub fn new(jobs: Arc<dyn CardOutbox>, ingress: Arc<Ingress>, sender: SlackSender) -> Self {
        Self {
            jobs,
            ingress,
            sender,
            unsettled: Mutex::new(Vec::new()),
        }
    }

    pub async fn tick(&self) -> Result<usize, Error> {
        self.resettle().await;
        let jobs = self.jobs.claim(BATCH).await?;
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
                    tracing::warn!(?error, "failed to claim approval card delivery jobs");
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
        // Fail closed if HTTP origin evidence cannot be read so HTTP cards never fall back to the channel path.
        let Ok(origin) = self.jobs.origin(job.key.nonce.clone()).await else {
            return Verdict::Leave;
        };
        let Ok(routes) = self.ingress.routes() else {
            return Verdict::Leave;
        };
        let Some(snapshot) = routes.snapshot(&job.profile) else {
            return Verdict::Leave;
        };
        let workspace = channel_workspace(snapshot, &job.binding);
        let Some(origin) = origin else {
            return match workspace.filter(|_| snapshot.members.contains_key(&job.owner)) {
                Some(workspace) => Verdict::Post(workspace.to_owned(), snapshot.revision.clone()),
                None => Verdict::Close,
            };
        };
        let pinned = self.ingress.pinned(&job.profile, &origin.revision);
        if let Some(why) = route_problem(pinned.as_deref(), snapshot, &origin, job) {
            return Verdict::Block(why);
        }
        // Require the workspace's current `team_id` to match the team verified at admission.
        let team_now = workspace.and_then(|alias| self.ingress.host().slack.workspaces.get(alias));
        match (workspace, team_now, origin.team.as_deref()) {
            (Some(workspace), Some(now), Some(then)) if now.team_id == then => {
                Verdict::Post(workspace.to_owned(), snapshot.revision.clone())
            }
            _ => Verdict::Block(ApprovalBlock::RouteChanged),
        }
    }

    async fn deliver_one(&self, job: CardJob) {
        let (workspace, revision) = match self.judge(&job).await {
            Verdict::Leave => return,
            Verdict::Close => {
                self.settle(job.key, Settlement::Outcome(CardOutcome::Unavailable))
                    .await;
                return;
            }
            Verdict::Block(why) => {
                self.settle(job.key, Settlement::Block(why)).await;
                return;
            }
            Verdict::Post(workspace, revision) => (workspace, revision),
        };
        let card = serde_json::from_str(&job.card.0)
            .ok()
            .and_then(|value| parse_card(&value).ok())
            .filter(|card| card.nonce == job.key.nonce.0);
        let Some(card) = card else {
            self.settle(job.key, Settlement::Outcome(CardOutcome::Unavailable))
                .await;
            return;
        };
        if self.jobs.begin(job.key.clone(), revision).await.is_err() {
            return;
        }
        let posted = tokio::time::timeout(
            SEND_TIMEOUT,
            self.sender
                .post_owner_card(&job.profile, &workspace, &job.owner, &card),
        )
        .await
        .unwrap_or(CardPost::Held);
        self.settle(job.key, Settlement::Outcome(outcome_of(posted)))
            .await;
    }

    async fn settle(&self, key: CardKey, settlement: Settlement) {
        self.unsettled
            .lock()
            .unwrap()
            .push(Unsettled { key, settlement });
        self.resettle().await;
    }

    async fn resettle(&self) {
        let waiting = std::mem::take(&mut *self.unsettled.lock().unwrap());
        for item in waiting {
            let recorded = match &item.settlement {
                Settlement::Outcome(outcome) => {
                    self.jobs.settle(item.key.clone(), outcome.clone()).await
                }
                Settlement::Block(why) => self.jobs.block(item.key.clone(), *why).await,
            };
            match recorded {
                Ok(()) => {}
                Err(Error::Unavailable) => self.unsettled.lock().unwrap().push(item),
                Err(error) => {
                    tracing::error!(?error, "approval card delivery settlement rejected");
                }
            }
        }
    }
}

fn outcome_of(posted: CardPost) -> CardOutcome {
    match posted {
        CardPost::Posted { team, dm, ts } => CardOutcome::Posted { team, dm, ts },
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
