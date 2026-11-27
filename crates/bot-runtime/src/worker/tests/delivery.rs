//! Scheduled delivery worker tests: draft freezing, resume without model rerun, and stale card disambiguation.

use super::fakes::*;
use crate::worker::{Delivery, Outcome};
use bot_core::{
    command::{CallResult, Failure, Finish, JsonPayload},
    id::*,
    port::{Error, Reply},
    schedule::{DeliveryDraft, DeliveryTarget, ScheduleDelivery, ScheduledReplies},
    state::{Conversation, ConversationKind, RunContext, RunOrigin},
};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct FakeReplies {
    drafts: Mutex<VecDeque<Result<Option<DeliveryDraft>, Error>>>,
    seen: Mutex<Vec<bool>>,
    fences: Mutex<VecDeque<Result<Option<DeliveryTarget>, Error>>>,
    fenced: Mutex<u32>,
}

fn draft() -> DeliveryDraft {
    DeliveryDraft {
        activation: Revision("set-1".into()),
        target: DeliveryTarget {
            binding: BindingId("slack".into()),
            conversation: Conversation {
                kind: ConversationKind::Direct,
                id: "D9".into(),
                thread: None,
            },
        },
        answers: vec![],
    }
}

impl ScheduledReplies for FakeReplies {
    fn receipt(&self, _: Attempt) -> Reply<'_, Option<JsonPayload>> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn draft(&self, _: Attempt, completion: Option<Finish>) -> Reply<'_, Option<DeliveryDraft>> {
        lock(&self.seen).push(completion.is_some());
        let next = lock(&self.drafts).pop_front().unwrap_or(Ok(None));
        Box::pin(async move { next })
    }
    fn target(&self, _: Attempt) -> Reply<'_, Option<DeliveryTarget>> {
        *lock(&self.fenced) += 1;
        let next = lock(&self.fences).pop_front().unwrap_or(Err(Error::Stale));
        Box::pin(async move { next })
    }
    fn finish_unsent(
        &self,
        _: bot_core::schedule::DeliveryFence,
        _: DeliveryTarget,
        _: bot_core::schedule::UnsentReason,
    ) -> Reply<'_, JsonPayload> {
        Box::pin(async { Err(Error::Unavailable) })
    }
    fn enqueue(&self, _: bot_core::ledger::StartCall) -> Reply<'_, ()> {
        Box::pin(async { Err(Error::Unavailable) })
    }
}

#[derive(Default)]
struct FakeRelease {
    script: Mutex<VecDeque<Result<CallResult, Error>>>,
    calls: Mutex<u32>,
}

impl ScheduleDelivery for FakeRelease {
    fn release(&self, _: Attempt) -> Reply<'_, CallResult> {
        *lock(&self.calls) += 1;
        let next = lock(&self.script)
            .pop_front()
            .unwrap_or(Err(Error::Unavailable));
        Box::pin(async move { next })
    }
}

struct Rigged {
    r: Rig,
    worker: crate::worker::Worker,
    replies: Arc<FakeReplies>,
    release: Arc<FakeRelease>,
}

fn rigged(model: Arc<ScriptedModel>) -> Rigged {
    let r = rig(model);
    let replies = Arc::new(FakeReplies::default());
    let release = Arc::new(FakeRelease::default());
    let worker = r.worker.clone().with_delivery(Delivery {
        replies: replies.clone(),
        release: release.clone(),
    });
    Rigged {
        r,
        worker,
        replies,
        release,
    }
}

fn scheduled(run: &str) -> RunContext {
    let mut ctx = agent_run(run);
    ctx.origin = RunOrigin::Schedule;
    ctx
}

#[tokio::test]
async fn a_final_answer_for_another_recipient_is_frozen_and_waits_for_the_owners_approval() {
    let model = ScriptedModel::new(vec![say("Morning summary")]);
    let t = rigged(model.clone());
    lock(&t.replies.drafts).push_back(Ok(None));
    lock(&t.replies.drafts).push_back(Ok(Some(draft())));
    lock(&t.release.script).push_back(Ok(CallResult::Awaiting(ApprovalId("n1".into()))));
    assert_eq!(t.worker.execute(scheduled("r1")).await, Outcome::Parked);
    assert_eq!(*lock(&t.replies.seen), [false, true]);
    assert_eq!(*lock(&t.release.calls), 1);
    assert!(lock(&t.r.runs.state).finished.is_empty());
    assert!(lock(&t.r.runs.state).failed.is_empty());
}

#[tokio::test]
async fn a_draft_frozen_under_an_older_set_still_reaches_the_release_entrance() {
    let t = rigged(ScriptedModel::new(vec![]));
    lock(&t.replies.drafts).push_back(Err(Error::Conflict));
    lock(&t.release.script).push_back(Ok(CallResult::Done(JsonPayload(
        r#"{"status":"delivery_unrouted"}"#.into(),
    ))));
    assert_eq!(t.worker.execute(scheduled("r1")).await, Outcome::Succeeded);
    assert_eq!(*lock(&t.release.calls), 1);
    assert!(lock(&t.r.runs.state).failed.is_empty());
    assert!(lock(&t.r.runs.state).finished.is_empty());

    let t = rigged(ScriptedModel::new(vec![]));
    lock(&t.replies.drafts).push_back(Err(Error::Conflict));
    lock(&t.release.script).push_back(Err(Error::Conflict));
    assert!(matches!(
        t.worker.execute(scheduled("r2")).await,
        Outcome::Failed(_)
    ));
    assert_eq!(
        lock(&t.r.runs.state).failed,
        [Failure::Permanent("delivery_stale")]
    );

    let t = rigged(ScriptedModel::new(vec![]));
    lock(&t.replies.drafts).push_back(Err(Error::Conflict));
    lock(&t.release.script).push_back(Err(Error::Unavailable));
    t.worker.execute(scheduled("r3")).await;
    assert_eq!(
        lock(&t.r.runs.state).failed,
        [Failure::Transient("delivery_unavailable")]
    );

    let t = rigged(ScriptedModel::new(vec![]));
    lock(&t.replies.drafts).push_back(Err(Error::Denied));
    t.worker.execute(scheduled("r4")).await;
    assert_eq!(*lock(&t.release.calls), 0);
}

#[tokio::test]
async fn a_resumed_attempt_reads_the_frozen_draft_without_running_the_model_again() {
    let model = ScriptedModel::new(vec![]);
    let t = rigged(model.clone());
    lock(&t.replies.drafts).push_back(Ok(Some(draft())));
    lock(&t.release.script).push_back(Ok(CallResult::Done(JsonPayload(r#"{"queued":2}"#.into()))));
    assert_eq!(t.worker.execute(scheduled("r1")).await, Outcome::Succeeded);
    assert!(
        model.requests().is_empty(),
        "model must not re-run when a frozen draft exists"
    );
    assert_eq!(*lock(&t.replies.seen), [false]);
    assert!(lock(&t.r.runs.state).finished.is_empty());
    assert_eq!(lock(&t.r.gateway.delivered).len(), 1);
}

#[tokio::test]
async fn runs_that_need_no_draft_finish_normally_and_other_origins_never_ask() {
    let model = ScriptedModel::new(vec![say("Confirmed")]);
    let t = rigged(model);
    assert_eq!(t.worker.execute(scheduled("r1")).await, Outcome::Succeeded);
    assert_eq!(*lock(&t.replies.seen), [false, true]);
    assert_eq!(lock(&t.r.runs.state).finished.len(), 1);
    assert_eq!(*lock(&t.release.calls), 0);

    let model = ScriptedModel::new(vec![say("reply"), say("reply")]);
    let t = rigged(model);
    assert_eq!(t.worker.execute(agent_run("r2")).await, Outcome::Succeeded);
    let mut child = scheduled("r3");
    child.root = RunId("r-root".into());
    assert_eq!(t.worker.execute(child).await, Outcome::Succeeded);
    assert!(lock(&t.replies.seen).is_empty());

    let t = rig(ScriptedModel::new(vec![say("reply")]));
    assert_eq!(t.worker.execute(scheduled("r4")).await, Outcome::Succeeded);
}

fn refusal(which: usize) -> (Result<CallResult, Error>, &'static str) {
    match which {
        0 => (Err(Error::Denied), "delivery_denied"),
        1 => (Err(Error::Missing), "delivery_missing"),
        2 => (Err(Error::Conflict), "delivery_stale"),
        3 => (Err(Error::Invalid), "card_unsupported"),
        4 => (Err(Error::Stale), ""),
        _ => (Err(Error::Unavailable), "delivery_unavailable"),
    }
}

#[tokio::test]
async fn refusals_become_fixed_failure_words_and_a_lost_claim_steps_aside() {
    for which in 0..6 {
        let (release, word) = refusal(which);
        let t = rigged(ScriptedModel::new(vec![]));
        lock(&t.replies.drafts).push_back(Ok(Some(draft())));
        lock(&t.release.script).push_back(release);
        let outcome = t.worker.execute(scheduled("r1")).await;
        let failed = lock(&t.r.runs.state).failed.clone();
        match word {
            "" => {
                assert_eq!(outcome, Outcome::Lost);
                assert!(failed.is_empty());
            }
            "delivery_unavailable" => {
                assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
                assert_eq!(failed, [Failure::Transient("delivery_unavailable")]);
            }
            word => {
                assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
                assert_eq!(failed.len(), 1, "{word}");
                assert!(
                    matches!(failed[0], Failure::Permanent(w) if w == word),
                    "{word}"
                );
            }
        }
        assert!(lock(&t.r.runs.state).finished.is_empty());
    }
    let t = rigged(ScriptedModel::new(vec![say("Reply with attachment")]));
    lock(&t.replies.drafts).push_back(Ok(None));
    lock(&t.replies.drafts).push_back(Err(Error::Invalid));
    t.worker.execute(scheduled("r2")).await;
    assert_eq!(
        lock(&t.r.runs.state).failed,
        [Failure::Permanent("card_unsupported")]
    );
    assert_eq!(*lock(&t.release.calls), 0);
}

async fn after_a_stale(
    fences: Vec<Result<Option<DeliveryTarget>, Error>>,
    release: Vec<Result<CallResult, Error>>,
) -> (Outcome, Vec<Failure>, u32, u32, usize) {
    let t = rigged(ScriptedModel::new(vec![]));
    lock(&t.replies.drafts).push_back(Ok(Some(draft())));
    lock(&t.replies.fences).extend(fences);
    lock(&t.release.script).extend(release);
    let outcome = t.worker.execute(scheduled("r1")).await;
    let failed = lock(&t.r.runs.state).failed.clone();
    let (calls, fenced) = (*lock(&t.release.calls), *lock(&t.replies.fenced));
    let finished = lock(&t.r.runs.state).finished.len();
    (outcome, failed, calls, fenced, finished)
}

#[tokio::test]
async fn a_stale_entrance_steps_aside_only_when_the_claim_fence_is_really_lost() {
    let (outcome, failed, calls, fenced, finished) =
        after_a_stale(vec![Err(Error::Stale)], vec![Err(Error::Stale)]).await;
    assert_eq!(outcome, Outcome::Lost);
    assert_eq!((calls, fenced, finished), (1, 1, 0));
    assert!(failed.is_empty());
}

#[tokio::test]
async fn a_stale_card_on_a_valid_claim_is_judged_again_on_that_claim() {
    let (outcome, failed, calls, fenced, finished) = after_a_stale(
        vec![Ok(Some(draft().target))],
        vec![
            Err(Error::Stale),
            Ok(CallResult::Awaiting(ApprovalId("n2".into()))),
        ],
    )
    .await;
    assert_eq!(outcome, Outcome::Parked);
    assert_eq!((calls, fenced, finished), (2, 1, 0));
    assert!(failed.is_empty());

    let (outcome, failed, calls, ..) = after_a_stale(
        vec![Ok(None)],
        vec![
            Err(Error::Stale),
            Ok(CallResult::Done(JsonPayload(r#"{"queued":1}"#.into()))),
        ],
    )
    .await;
    assert_eq!(outcome, Outcome::Succeeded);
    assert_eq!(calls, 2);
    assert!(failed.is_empty());
}

#[tokio::test]
async fn an_expired_approval_ends_as_a_denied_delivery_not_as_a_lost_claim() {
    let (outcome, failed, calls, ..) = after_a_stale(
        vec![Ok(Some(draft().target))],
        vec![Err(Error::Stale), Err(Error::Denied)],
    )
    .await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!(calls, 2);
    assert!(matches!(
        failed[..],
        [Failure::Permanent("delivery_denied")]
    ));
}

#[tokio::test]
async fn a_card_that_stays_stale_on_a_valid_claim_is_a_transient_failure_never_a_lost_claim() {
    let (outcome, failed, calls, fenced, finished) = after_a_stale(
        vec![Ok(Some(draft().target))],
        vec![Err(Error::Stale), Err(Error::Stale), Err(Error::Stale)],
    )
    .await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!((calls, fenced, finished), (2, 1, 0));
    assert!(matches!(
        failed[..],
        [Failure::Transient("delivery_card_stale")]
    ));
}

#[tokio::test]
async fn a_fence_that_cannot_be_read_is_a_transient_failure_and_the_entrance_is_not_called_again() {
    let (outcome, failed, calls, fenced, _) =
        after_a_stale(vec![Err(Error::Unavailable)], vec![Err(Error::Stale)]).await;
    assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
    assert_eq!((calls, fenced), (1, 1));
    assert!(matches!(
        failed[..],
        [Failure::Transient("delivery_unavailable")]
    ));
}
