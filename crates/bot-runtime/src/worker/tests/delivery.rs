//! 예약 회신 흐름: 목적지가 다른 예약 발화는 초안을 동결하고 소유자가 승인한 뒤 저장소가 끝낸다. 재개하면 모델을 다시 돌리지 않는다.

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
    /// 부른 `draft` 호출: 최종 답을 동결했는가(`Some`)
    seen: Mutex<Vec<bool>>,
    /// 고정 수신자 읽기(= claim·활성 집합 펜스)의 답. 비어 있으면 펜스를 잃은 것(`Stale`)이다.
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
    let model = ScriptedModel::new(vec![say("아침 요약")]);
    let t = rigged(model.clone());
    lock(&t.replies.drafts).push_back(Ok(None)); // 재개 확인: 아직 동결한 초안이 없다
    lock(&t.replies.drafts).push_back(Ok(Some(draft()))); // 최종 답 동결
    lock(&t.release.script).push_back(Ok(CallResult::Awaiting(ApprovalId("n1".into()))));
    assert_eq!(t.worker.execute(scheduled("r1")).await, Outcome::Parked);
    assert_eq!(*lock(&t.replies.seen), [false, true]);
    assert_eq!(*lock(&t.release.calls), 1);
    // 일반 완료는 하지 않는다(원래 대화로 답이 돌지 않는다)
    assert!(lock(&t.r.runs.state).finished.is_empty());
    assert!(lock(&t.r.runs.state).failed.is_empty());
}

#[tokio::test]
async fn a_draft_frozen_under_an_older_set_still_reaches_the_release_entrance() {
    // 활성 집합이 바뀌어 `draft`가 `Conflict`다. 입구가 수신자 철회를 가려 미발송 영수증을 돌려주면 그대로 끝난다(`delivery_stale`로 가려지지 않는다)
    let t = rigged(ScriptedModel::new(vec![]));
    lock(&t.replies.drafts).push_back(Err(Error::Conflict));
    lock(&t.release.script).push_back(Ok(CallResult::Done(JsonPayload(
        r#"{"status":"delivery_unrouted"}"#.into(),
    ))));
    assert_eq!(t.worker.execute(scheduled("r1")).await, Outcome::Succeeded);
    assert_eq!(*lock(&t.release.calls), 1);
    assert!(lock(&t.r.runs.state).failed.is_empty());
    assert!(lock(&t.r.runs.state).finished.is_empty());
    // 수신자가 그대로면 입구도 `Conflict`다: 옛 집합의 초안은 보내지 않는다(영구 실패 `delivery_stale`)
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
    // 철회를 가리는 중에 확인하지 못했다면 재시도한다(발생을 버리지 않는다)
    let t = rigged(ScriptedModel::new(vec![]));
    lock(&t.replies.drafts).push_back(Err(Error::Conflict));
    lock(&t.release.script).push_back(Err(Error::Unavailable));
    t.worker.execute(scheduled("r3")).await;
    assert_eq!(
        lock(&t.r.runs.state).failed,
        [Failure::Transient("delivery_unavailable")]
    );
    // 다른 사정으로 `draft`가 실패하면 입구를 부르지 않는다
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
    assert!(model.requests().is_empty(), "모델을 다시 돌리지 않는다");
    assert_eq!(*lock(&t.replies.seen), [false]);
    // 저장소가 등록과 완료를 한 커밋으로 마쳤으니 워커가 다시 끝내지 않고 목적지 회신을 서둘러 내보낸다
    assert!(lock(&t.r.runs.state).finished.is_empty());
    assert_eq!(lock(&t.r.gateway.delivered).len(), 1);
}

#[tokio::test]
async fn runs_that_need_no_draft_finish_normally_and_other_origins_never_ask() {
    // 조용한 성공·원래 대화로 가는 답: 초안이 필요 없다
    let model = ScriptedModel::new(vec![say("확인했어요")]);
    let t = rigged(model);
    assert_eq!(t.worker.execute(scheduled("r1")).await, Outcome::Succeeded);
    assert_eq!(*lock(&t.replies.seen), [false, true]);
    assert_eq!(lock(&t.r.runs.state).finished.len(), 1);
    assert_eq!(*lock(&t.release.calls), 0);
    // 사용자 요청·자식 run은 초안을 묻지 않는다
    let model = ScriptedModel::new(vec![say("답"), say("답")]);
    let t = rigged(model);
    assert_eq!(t.worker.execute(agent_run("r2")).await, Outcome::Succeeded);
    let mut child = scheduled("r3");
    child.root = RunId("r-root".into());
    assert_eq!(t.worker.execute(child).await, Outcome::Succeeded);
    assert!(lock(&t.replies.seen).is_empty());
    // 입구가 없는 호스트에서는 일반 완료다
    let t = rig(ScriptedModel::new(vec![say("답")]));
    assert_eq!(t.worker.execute(scheduled("r4")).await, Outcome::Succeeded);
}

/// 승인 입구의 거절 하나와 그에 따른 (결말, 실패 단어).
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
    // 승인 문서·카드로 만들 수 없는 답은 동결 단계에서 영구 실패이고 승인 입구를 부르지 않는다
    let t = rigged(ScriptedModel::new(vec![say("첨부가 있는 답")]));
    lock(&t.replies.drafts).push_back(Ok(None));
    lock(&t.replies.drafts).push_back(Err(Error::Invalid));
    t.worker.execute(scheduled("r2")).await;
    assert_eq!(
        lock(&t.r.runs.state).failed,
        [Failure::Permanent("card_unsupported")]
    );
    assert_eq!(*lock(&t.release.calls), 0);
}

/// 입구가 `Stale`을 돌려준 뒤의 흐름을 확인한다: `fences`는 펜스 읽기의 답, `release`는 입구의 답들이다.
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
    // 펜스를 잃었다: 진짜 소유권 상실이다. 입구는 한 번만 불렀고 아무것도 쓰지 않는다
    let (outcome, failed, calls, fenced, finished) =
        after_a_stale(vec![Err(Error::Stale)], vec![Err(Error::Stale)]).await;
    assert_eq!(outcome, Outcome::Lost);
    assert_eq!((calls, fenced, finished), (1, 1, 0));
    assert!(failed.is_empty());
}

#[tokio::test]
async fn a_stale_card_on_a_valid_claim_is_judged_again_on_that_claim() {
    // 카드가 대체돼 `Stale`이었다. 펜스는 유효하니 같은 claim에서 한 번 더 판정하고, 브로커가 새 카드를 열면 승인을 다시 기다린다(시도·실패 없음)
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
    // 이미 끝난 것으로 읽히면(저장소가 끝냈다) 그대로 끝이다
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
    // 다시 판정한 브로커가 승인 만료로 닫았다(`Denied`): 영구 실패 단어이고 소유권 상실이 아니다
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
    // 두 번 판정해도 카드가 낡았다(시계 어긋남 같은 일시 사정): 소유권 상실이 아니라 시도를 쓰는 일시 실패다. 입구는 두 번이 끝이다
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
