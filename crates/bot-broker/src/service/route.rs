//! 명시 목적지(다른 수신자)의 호스트 등록부 대조. 예약 생성·발송 승인·승인 결정이 같은 규칙으로 **현재** 등록부를 본다
//! (외부 서비스 조회는 없다: 같은 프로필의 운영자 등록부 하나가 근거다).
//!
//! - 생성: 모델이 지정한 수신자를 정규 전송 경로(`팀ID/채널ID`)로 확정한 값으로만 카드·해시·저장 명세를 만든다.
//! - 판정·시작·발송: 이미 정규 경로로 고정된 수신자가 지금 등록부에서도 **같은 경로**일 때만 이어 간다(빠졌거나 다른 경로로 재지정됐으면 멈춘다).
//! - 카드에 보인 문서(생성의 `인자`, 발송의 `전송 초안`)에서 수신자를 읽는다: 그 문서의 SHA-256이 승인에 묶인 해시와 같을 때만 믿는다.

use super::{BrokerService, call::Flow};
use crate::{approval::card_value, canon::sha256};
use bot_core::{
    command::{CallResult, JsonPayload},
    id::{Attempt, BindingId, CallId, ProfileId},
    ledger::{ApprovalState, CallPhase},
    port::Error,
    schedule::{DELIVER_CALL, DELIVER_TOOL, DeliveryFence, DeliveryTarget, UnsentReason},
    state::{Conversation, ConversationKind},
};
use serde_json::Value;

/// 같은 바인딩·종류·ID·스레드인가(정규 경로 비교).
pub(super) fn same_route(a: &DeliveryTarget, b: &DeliveryTarget) -> bool {
    a.binding == b.binding
        && a.conversation.kind == b.conversation.kind
        && a.conversation.id == b.conversation.id
        && a.conversation.thread == b.conversation.thread
}

/// 승인 문서의 `target`/`destination` JSON(`{"binding","conversation":{"kind","id","thread"}}`)에서 수신자를 읽는다.
fn target_of(value: &Value) -> Option<DeliveryTarget> {
    let conversation = value.get("conversation")?;
    let kind = match conversation.get("kind")?.as_str()? {
        "direct" => ConversationKind::Direct,
        "channel" => ConversationKind::Channel,
        _ => return None,
    };
    Some(DeliveryTarget {
        binding: BindingId(value.get("binding")?.as_str()?.to_owned()),
        conversation: Conversation {
            kind,
            id: conversation.get("id")?.as_str()?.to_owned(),
            thread: match conversation.get("thread")? {
                Value::Null => None,
                Value::String(thread) => Some(thread.clone()),
                _ => return None,
            },
        },
    })
}

/// 카드의 `label` 행에 담긴 JSON 문서와 그 안의 `key`(수신자 객체). 문서의 SHA-256이 승인한 해시와 다르면 `Err`(믿지 않는다).
/// 문서에 수신자가 없으면 `Ok(None)`이다.
fn card_target(
    card: &JsonPayload,
    label: &str,
    key: &str,
    approved: [u8; 32],
) -> Result<Option<DeliveryTarget>, Error> {
    let document = card_value(card, label).ok_or(Error::Invalid)?;
    if sha256(&document) != approved {
        return Err(Error::Invalid);
    }
    let value: Value = crate::json::parse(&document).ok_or(Error::Invalid)?;
    match value.get(key) {
        None => Ok(None),
        Some(target) => target_of(target).map(Some).ok_or(Error::Invalid),
    }
}

/// 생성 승인 카드(`인자`)의 명시 목적지. 없으면 원래 대화다.
pub(super) fn created_destination(
    card: &JsonPayload,
    approved: [u8; 32],
) -> Result<Option<DeliveryTarget>, Error> {
    card_target(card, "인자", "destination", approved)
}

/// 발송 승인 카드(`전송 초안`)의 수신자.
pub(super) fn delivery_destination(
    card: &JsonPayload,
    approved: [u8; 32],
) -> Result<DeliveryTarget, Error> {
    card_target(
        card,
        bot_core::schedule::DELIVERY_CARD_LABEL,
        "target",
        approved,
    )?
    .ok_or(Error::Invalid)
}

impl BrokerService {
    /// 수신자를 등록부로 정규 경로에 확정한다. 등록부를 붙이지 않았으면 `Unavailable`이 아니라 `Missing`이다(지원하지 않는다).
    pub(super) fn route(
        &self,
        profile: &ProfileId,
        target: &DeliveryTarget,
    ) -> Result<DeliveryTarget, Error> {
        self.routes
            .as_ref()
            .ok_or(Error::Missing)?
            .resolve(profile, target)
    }

    /// 수신자가 지금 등록부에서 빠졌거나 다른 경로가 됐다(확정). 예약 회신 **전송**이면 이 발생을 «미발송»으로 끝낸다 — 원래 대화에 고정 안내 한 건·초안 종결·
    /// 옛 카드 무효·미발송 영수증이 저장소의 한 커밋이다. 그 영수증이 이번 호출의 결과다. 수신자를 지정한 **생성**이면 호출만 거절한다(만들지 않는다).
    pub(super) async fn route_gone(
        &self,
        flow: &Flow,
        target: &DeliveryTarget,
    ) -> Result<CallResult, Error> {
        if flow.tool.0 == DELIVER_TOOL {
            return self
                .finish_unsent(&flow.attempt, target, UnsentReason::Unrouted)
                .await;
        }
        let _ = self
            .note(
                &flow.attempt.profile,
                &flow.ctx.principal,
                "call.refuse",
                &flow.tool,
                Some(flow.sealed.sha256),
                self.route_word(),
            )
            .await;
        Err(Error::Denied)
    }

    /// 이 발생을 미발송으로 끝낸다(`Claim` 펜스: 현재 claim을 쥔 시도). 영수증이 호출의 결과다.
    pub(super) async fn finish_unsent(
        &self,
        attempt: &Attempt,
        target: &DeliveryTarget,
        reason: UnsentReason,
    ) -> Result<CallResult, Error> {
        self.replies
            .finish_unsent(
                DeliveryFence::Claim(attempt.clone()),
                target.clone(),
                reason,
            )
            .await
            .map(CallResult::Done)
    }

    /// 소유자가 이 발생의 전송 카드를 이미 거절했거나 소비 전에 철회했는가(저장된 결정). 호출이나 카드가 아직 없으면 결정도 없다.
    pub(super) async fn owner_rejected(&self, attempt: &Attempt) -> Result<bool, Error> {
        let ledger = &self.parts.ledger;
        let call = CallId(DELIVER_CALL.into());
        let Some(CallPhase::Open {
            approval: Some(nonce),
        }) = ledger
            .lookup(attempt.clone(), call)
            .await?
            .map(|record| record.phase)
        else {
            return Ok(false);
        };
        let stored = ledger.approval(nonce).await?;
        Ok(matches!(
            stored.state,
            ApprovalState::Denied | ApprovalState::Revoked
        ))
    }

    /// 수신자가 지금 등록부에 없거나(확정) 확인하지 못했다(일시). **기록된 소유자 결정이 먼저다**: 이미 거절·철회했다면 등록부가 어떻든 이 발생은 «거절»로
    /// 끝난다(안내 없음). 결정이 없을 때만 빠졌으면 «미발송»으로 끝내고, 확인하지 못했으면 같은 발생을 다시 시도한다.
    pub(super) async fn unsettled_route(
        &self,
        attempt: &Attempt,
        target: &DeliveryTarget,
        route: Route,
    ) -> Result<CallResult, Error> {
        if self.owner_rejected(attempt).await? {
            return self
                .finish_unsent(attempt, target, UnsentReason::Rejected)
                .await;
        }
        match route {
            Route::Gone => {
                self.finish_unsent(attempt, target, UnsentReason::Unrouted)
                    .await
            }
            Route::Same | Route::Unsure => Err(Error::Unavailable),
        }
    }

    /// 수신자를 확인하지 못했을 때 감사에 남기는 고정 낱말: 등록부를 붙이지 않았다 / 등록부에 없거나 다른 경로로 재지정됐다.
    pub(super) fn route_word(&self) -> &'static str {
        if self.routes.is_some() {
            "destination_unregistered"
        } else {
            "destination_unsupported"
        }
    }

    /// 이미 정규 경로로 고정된 수신자를 지금 등록부로 다시 본 결과. **확정**(같은 경로·빠졌거나 다른 경로로 재지정됨)과 **확인하지 못함**(등록부가
    /// 없거나 일시 오류)을 가른다: 확인하지 못했을 때는 아무것도 하지 않고 같은 발생을 다시 시도한다(발생을 버리지 않는다).
    pub(super) fn route_state(&self, profile: &ProfileId, target: &DeliveryTarget) -> Route {
        match self.route(profile, target) {
            Ok(now) if same_route(target, &now) => Route::Same,
            // 등록부가 이 수신자를 거절하거나 같은 바인딩의 다른 실제 수신자를 가리킨다
            Ok(_) | Err(Error::Denied | Error::Invalid) => Route::Gone,
            Err(_) => Route::Unsure,
        }
    }
}

/// 수신자를 지금 호스트 등록부로 본 결과.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Route {
    /// 봉인한 그대로의 같은 경로다.
    Same,
    /// 빠졌거나 다른 경로로 재지정됐다(확정).
    Gone,
    /// 확인하지 못했다(등록부를 붙이지 않았거나 일시 오류).
    Unsure,
}
