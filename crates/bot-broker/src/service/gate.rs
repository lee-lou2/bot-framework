use super::{BrokerService, call::Flow, route::Route};
use crate::{
    approval::{new_nonce, render_card, render_delivery_card, render_memory_card, same_binding},
    classify::Class,
};
use bot_core::{
    command::*,
    id::*,
    ledger::*,
    port::Error,
    schedule::{DELIVER_TOOL, UnsentReason},
    state::*,
};
use std::time::SystemTime;

pub(super) enum Gate {
    /// 실행해도 된다. 승인이 필요했다면 소비할 승인(기대 결합값과 이유).
    Go(Option<Box<(Approval, Vec<String>)>>),
    /// 실행하지 않고 이 결과를 돌려준다(승인 대기).
    Return(CallResult),
}

/// 지금 상태로 다시 계산한 승인 결합값의 재료. 호출 경로와 승인 클릭 경로가 같은 계산을 쓴다.
pub(super) struct Binding<'a> {
    pub attempt: &'a Attempt,
    pub call: &'a CallId,
    pub requester: &'a PrincipalId,
    pub approver: &'a PrincipalId,
    pub class: &'a Class,
    pub policy_revision: &'a Revision,
    pub tool: &'a ToolId,
    pub tool_revision: &'a Revision,
    pub args_sha256: [u8; 32],
}

impl Binding<'_> {
    /// nonce와 만료는 비어 있다(저장된 승인의 값을 쓰거나 새로 정한다).
    pub fn approval(&self) -> Approval {
        Approval {
            nonce: ApprovalId(String::new()),
            attempt: self.attempt.clone(),
            call: self.call.clone(),
            requester: self.requester.clone(),
            approver: self.approver.clone(),
            risk: self.class.risk,
            policy_revision: self.policy_revision.clone(),
            tool: self.tool.clone(),
            tool_revision: self.tool_revision.clone(),
            args_sha256: self.args_sha256,
            expires_at: SystemTime::UNIX_EPOCH,
        }
    }
}

impl BrokerService {
    /// 승인이 필요한 호출을 가른다. 승인된 호출은 결합값이 지금과 같고 만료 전일 때만 통과한다.
    pub(super) async fn gate(
        &self,
        flow: &Flow,
        class: &Class,
        pointer: Option<ApprovalId>,
    ) -> Result<Gate, Error> {
        if !class.needs_approval() {
            return Ok(Gate::Go(None));
        }
        let Some(approver) = flow.approver.as_ref() else {
            return self.deny(flow, "no_approver").await;
        };
        let reasons = class.codes();
        let expected = Binding {
            attempt: &flow.attempt,
            call: &flow.call,
            requester: &flow.ctx.principal,
            approver,
            class,
            policy_revision: &flow.snapshot.policy_revision,
            tool: &flow.tool,
            tool_revision: &flow.resolved.tool.revision,
            args_sha256: flow.sealed.sha256,
        }
        .approval();
        let now = self.parts.clock.now();
        if let Some(nonce) = pointer {
            let stored = self.parts.ledger.approval(nonce.clone()).await?;
            let live = now < stored.approval.expires_at;
            let same = same_binding(&stored.approval, &expected) && stored.reasons == reasons;
            match stored.state {
                ApprovalState::Denied | ApprovalState::Revoked => {
                    // 예약 회신 전송에서 소유자가 거절했거나 소비 전에 승인을 철회했다: 이 발생을 «거절»로 끝낸다(영수증이 결과, 본문은 보내지 않는다).
                    if flow.tool.0 == DELIVER_TOOL
                        && let Some(target) = &flow.route
                    {
                        return self
                            .finish_unsent(&flow.attempt, target, UnsentReason::Rejected)
                            .await
                            .map(Gate::Return);
                    }
                    return self.deny(flow, "approval_denied").await;
                }
                ApprovalState::Expired => return self.deny(flow, "approval_expired").await,
                ApprovalState::Pending | ApprovalState::Approved if same && !live => {
                    return self.deny(flow, "approval_expired").await;
                }
                ApprovalState::Pending if same => {
                    return Ok(Gate::Return(CallResult::Awaiting(nonce)));
                }
                ApprovalState::Approved if same => {
                    let consumable = Approval {
                        nonce: stored.approval.nonce,
                        expires_at: stored.approval.expires_at,
                        ..expected
                    };
                    return Ok(Gate::Go(Some(Box::new((consumable, reasons)))));
                }
                // 소비된 승인, 결합값이 바뀐 승인, 대체된 승인은 쓸 수 없다. 새로 연다.
                _ => {}
            }
        }
        // 다른 수신자로 가는 호출(수신자를 지정한 생성·예약 회신 전송)은 카드를 열기 **직전**에 현재 등록부로 그 수신자를 다시 본다(앞선 확인 뒤 철회됐으면
        // 카드를 열지 않는다). 확인하지 못하면 카드도 기록도 없이 같은 호출을 다시 시도하게 한다.
        if let Some(target) = &flow.route {
            match self.route_state(&flow.attempt.profile, target) {
                Route::Same => {}
                Route::Gone => return self.route_gone(flow, target).await.map(Gate::Return),
                Route::Unsure => return Err(Error::Unavailable),
            }
        }
        let nonce = new_nonce(self.parts.entropy.as_ref()).ok_or(Error::Unavailable)?;
        let card = match (&flow.memory, &flow.delivery) {
            // 예약 회신 전송: 수신자와 모든 답이 든 승인 문서 전체가 카드에 담긴다(해시가 이 문서에서 나왔다).
            (_, Some(document)) => {
                render_delivery_card(&nonce, class, &flow.ctx.principal, approver, document)
            }
            // 기억 공유: 소유자가 볼 본문 전체가 카드에 담긴다(승인 해시는 이 본문에서 나왔다).
            (Some((id, text)), None) => {
                render_memory_card(&nonce, class, &flow.ctx.principal, approver, id, text)
            }
            (None, None) => render_card(
                &nonce,
                &flow.tool,
                class,
                &flow.ctx.principal,
                approver,
                &flow.sealed.canonical,
                flow.sealed.destination.as_ref(),
            ),
        };
        let card = match card {
            Ok(card) => card,
            Err(_) => return self.deny(flow, "card_too_large").await,
        };
        let approval = Approval {
            nonce: nonce.clone(),
            expires_at: now + self.cfg.approval_ttl,
            ..expected
        };
        self.parts
            .ledger
            .open_approval(OpenApproval {
                approval,
                reasons,
                card,
            })
            .await?;
        Ok(Gate::Return(CallResult::Awaiting(nonce)))
    }
}
