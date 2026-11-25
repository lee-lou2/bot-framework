//! 예약 회신: 목적지가 다른 예약 발화는 최종 답을 한 번 동결한 초안(`ScheduledReplies::draft`)을 소유자가 승인해야 끝난다. 이 워커는 초안을 동결하고
//! 브로커의 전용 입구(`ScheduleDelivery::release`)를 부를 뿐이다: 승인 대기는 저장소가 run을 멈추고, 승인하면 같은 입구를 다시 불러
//! 저장소가 승인 소비·목적지 등록·run 완료를 한 커밋으로 마친다. 재개한 시도는 모델을 다시 돌리지 않고 동결한 초안만 읽는다.

use super::{Flow, Worker};
use bot_core::{
    command::{CallResult, Failure, Finish},
    port::Error,
    schedule::{ScheduleDelivery, ScheduledReplies},
    state::{RunContext, RunOrigin},
};
use std::sync::Arc;

/// 초안 저장소(`draft`)와 브로커의 회신 입구(`release`).
#[derive(Clone)]
pub struct Delivery {
    pub replies: Arc<dyn ScheduledReplies>,
    pub release: Arc<dyn ScheduleDelivery>,
}

impl Worker {
    /// 이 run이 동결한 회신 초안으로 끝나야 하면 그 흐름을 돌려준다. `None`이면 일반 실행·일반 완료다(초안이 필요 없다).
    /// `finish`가 `Some`이면 방금 만든 최종 결과·답을 동결하고, `None`이면 재개한 시도가 이미 동결한 초안만 읽는다.
    pub(super) async fn delivery(&self, ctx: &RunContext, finish: Option<Finish>) -> Option<Flow> {
        let delivery = self.delivery.as_ref()?;
        // 사용자 예약이 발화한 루트 run만 목적지를 가진다(위임한 자식은 부모만 답한다)
        if ctx.origin != RunOrigin::Schedule || ctx.root != ctx.attempt.run {
            return None;
        }
        match delivery.replies.draft(ctx.attempt.clone(), finish).await {
            Ok(None) => None,
            // 동결한 초안이 있다. 활성 집합이 바뀌어 옛 집합에서 동결한 초안(`Conflict`)도 입구로 넘긴다: 본문은 보내지 않지만 그 사이 수신자가 철회됐는지는
            // 입구가 고정 수신자만 읽어 먼저 가른다(철회면 «미발송» 영수증, 아니면 입구가 예전처럼 `Conflict`를 돌려줘 `delivery_stale`이다).
            Ok(Some(_)) | Err(Error::Conflict) => Some(
                match settled(delivery.release.release(ctx.attempt.clone()).await) {
                    // 입구가 `Stale`을 돌려줬다. 이것만으로는 소유권 상실이 아니다: 소비 직전에 승인이 만료됐거나 다른 카드로 대체돼도 저장소는 `Stale`이다.
                    Err(Error::Stale) => self.rejudge(delivery, ctx).await,
                    Ok(flow) => flow,
                    Err(error) => refused(error, "delivery_unavailable"),
                },
            ),
            Err(error) => Some(refused(error, "draft_unavailable")),
        }
    }

    /// `Stale`의 까닭을 가른다. 초안을 읽는 입구와 같은 펜스(현재 claim·활성 집합)를 고정 수신자 읽기로 확인한다:
    /// - 펜스를 잃었다 → 진짜 소유권 상실이다. 물러난다(리스 인계 규칙).
    /// - 펜스가 유효하다 → `Stale`은 카드 때문이다. 이 claim에서 브로커가 지금 상태로 **한 번 더** 판정하게 한다:
    ///   만료된 승인은 승인 만료로 닫히고(`delivery_denied`, 시도를 쓰지 않는다), 대체된 카드는 새 카드를 열어 승인을 다시 기다린다.
    ///   다시 판정해도 카드가 계속 낡았다면(시계 어긋남 같은 일시 사정) 소유권 상실이 아니라 시도를 쓰는 일시 실패다.
    async fn rejudge(&self, delivery: &Delivery, ctx: &RunContext) -> Flow {
        match delivery.replies.target(ctx.attempt.clone()).await {
            Err(Error::Stale) => return Flow::Lost,
            Err(error) => return refused(error, "delivery_unavailable"),
            Ok(_) => {}
        }
        match settled(delivery.release.release(ctx.attempt.clone()).await) {
            Ok(flow) => flow,
            Err(Error::Stale) => Flow::Fail(Failure::Transient("delivery_card_stale")),
            Err(error) => refused(error, "delivery_unavailable"),
        }
    }
}

/// 입구가 끝낸 결과를 흐름으로 옮긴다. 아직 끝나지 않은 것(`Err`)은 호출자가 가른다.
fn settled(result: Result<CallResult, Error>) -> Result<Flow, Error> {
    result.map(|result| match result {
        // 저장소가 이미 끝냈다: 등록한 영수증이든 미발송(수신자 철회·소유자 거절) 영수증이든 워커가 다시 끝내지 않는다
        CallResult::Done(_) => Flow::Delivered,
        // 승인 대기·보류: 저장소가 run을 멈췄다
        CallResult::Awaiting(_) | CallResult::Held(_) | CallResult::Passed => Flow::Parked,
    })
}

/// 초안·승인 단계의 실패를 흐름으로 옮긴다(고정 어휘). 다시 해도 같은 것은 영구 실패이고, 저장소·브로커 사정만 재시도한다.
fn refused(error: Error, transient: &'static str) -> Flow {
    match error {
        // 보낼 수 없는 답(승인 문서·카드로 만들 수 없는 첨부·한도)
        Error::Invalid => Flow::Fail(Failure::Permanent("card_unsupported")),
        // 소유자가 거절했거나 승인이 만료·권한이 철회됐다: 보내지 않는다
        Error::Denied => Flow::Fail(Failure::Permanent("delivery_denied")),
        // 동결한 초안이 없다
        Error::Missing => Flow::Fail(Failure::Permanent("delivery_missing")),
        // 동결한 초안과 지금의 활성 집합이 달라졌다: 이 초안은 보낼 수 없다
        Error::Conflict => Flow::Fail(Failure::Permanent("delivery_stale")),
        // 이 시도가 소유자가 아니다: 물러난다
        Error::Stale => Flow::Lost,
        Error::Unavailable => Flow::Fail(Failure::Transient(transient)),
    }
}
