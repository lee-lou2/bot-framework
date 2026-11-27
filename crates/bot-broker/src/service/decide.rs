//! 승인자의 승인·거절 결정 처리(`Broker::decide`)와 카드 조건의 재대조.
//!
//! 카드 개시 뒤 정책·도구 판본·승인자·수신 경로가 바뀌었으면 결정을 반영하지 않고 옛 승인을 대체하거나
//! 미발송으로 종결한다. 승인 소비와 실제 도구 실행은 깨어난 실행이 다시 들어올 때 `gate`·`exec`가 맡는다.

use super::{
    BrokerService,
    call::Decl,
    deliver::{deliver_approver, deliver_class, deliver_tool},
    gate::Binding,
    route::{Route, created_destination, delivery_destination},
    schedule::{self, CREATE_TOOL, REVISION as SCHEDULE_REVISION, create_tool_to},
    share::{SHARE_REVISION, SHARE_TOOL, share_approver, share_class, share_tool},
};
use crate::{
    approval::same_binding,
    approver::{approver_for, openai},
    classify::classify,
};
use bot_core::{
    id::*,
    ledger::*,
    port::Error,
    profile::ApprovalRoute,
    schedule::{DELIVER_REVISION, DELIVER_TOOL, DeliveryFence, DeliveryTarget, UnsentReason},
    scope::authorize,
};

/// 승인 화면이 보여 준 조건을 지금 다시 본 결과.
enum Check {
    /// 모두 그대로다.
    Unchanged,
    /// 바뀌었다: 옛 버튼은 무효다.
    Changed,
    /// 전송 카드의 수신자가 지금 등록부에서 빠졌거나 다른 경로가 됐다(확정): 이 발생을 미발송으로 끝낸다.
    Unrouted(DeliveryTarget),
}

/// Slack 경로의 카드는 게시를 확인한 영수증(팀·DM·ts)이 있어야 하고, 그 영수증이 지정한 사람·전달 바인딩·프로필의 것이어야 누를 수 있다. 영수증이 아직 없으면 카드가
/// 열리지 않은 것이라 `Conflict`(저장소도 같은 이유로 거절한다), 다른 사람·바인딩의 것이면 `Stale`이다. 어느 쪽이든 이 클릭만 거절하고 카드와 승인은 그대로다
/// (위조·이른 클릭이 정상 카드를 닫지 못한다). 로컬 경로는 영수증이 없다(본인 인증된 관리 화면).
fn receipt_matches(stored: &StoredApproval, found: &openai::Origin) -> Result<(), Error> {
    let ApprovalRoute::Slack { person, binding } = &found.route else {
        return Ok(());
    };
    match &stored.delivery {
        Some(receipt)
            if receipt.owner == *person
                && receipt.binding == *binding
                && receipt.profile == stored.approval.attempt.profile =>
        {
            Ok(())
        }
        None => Err(Error::Conflict),
        Some(_) => Err(Error::Stale),
    }
}

impl BrokerService {
    /// 승인자의 결정을 받는다. 승인 화면을 띄운 뒤 조건이 바뀌었으면 승인하지 않는다(옛 버튼 거절).
    /// 승인 대기 중인 호출은 run이 같은 호출 ID로 다시 들어올 때 새 조건을 표시한 새 승인을 연다.
    pub(super) async fn decide_flow(
        &self,
        nonce: ApprovalId,
        actor: PrincipalId,
        allow: bool,
    ) -> Result<(), Error> {
        let ledger = &self.parts.ledger;
        let stored = ledger.approval(nonce.clone()).await?;
        let approval = &stored.approval;
        // 지정된 승인자만, 그것도 사람만 승인한다. 모델과 서비스 주체는 승인할 수 없다.
        if actor != approval.approver || !actor.is_human() {
            let _ = self
                .note(
                    &approval.attempt.profile,
                    &actor,
                    "approval.decide",
                    &approval.tool,
                    Some(approval.args_sha256),
                    "actor_mismatch",
                )
                .await;
            return Err(Error::Denied);
        }
        match (stored.state, allow) {
            // 같은 결정을 다시 누르면 기존 결과를 본다. 새 실행은 만들지 않는다.
            (ApprovalState::Approved | ApprovalState::Consumed, true)
            | (ApprovalState::Denied | ApprovalState::Revoked, false) => return Ok(()),
            (ApprovalState::Consumed, false) | (ApprovalState::Denied, true) => {
                return Err(Error::Conflict);
            }
            (ApprovalState::Expired | ApprovalState::Superseded | ApprovalState::Revoked, _) => {
                return Err(Error::Stale);
            }
            // 승인한 뒤 실행 전에는 철회할 수 있다.
            (ApprovalState::Approved, false) => {
                return ledger
                    .decide(Decision {
                        nonce,
                        actor,
                        allow,
                    })
                    .await
                    .map(|_| ());
            }
            (ApprovalState::Pending, _) => {}
        }
        if self.parts.clock.now() >= approval.expires_at {
            return Err(Error::Stale);
        }
        // 옛 세대·취소된 run의 버튼은 `approval_context`가 `Stale`로 거절한다.
        let ctx = ledger
            .approval_context(nonce.clone(), actor.clone())
            .await?;
        if allow {
            match self.conditions_unchanged(&stored, &ctx).await {
                Ok(Check::Unchanged) => {}
                Ok(Check::Changed) | Err(Error::Denied | Error::Missing | Error::Invalid) => {
                    ledger.supersede(nonce, actor).await?;
                    return Err(Error::Stale);
                }
                // 승인 전송 카드의 수신자가 등록부에서 빠졌다: 옛 버튼을 무효로 하는 일과 이 발생의 «미발송» 종결(원래 대화 안내 한 건·미발송 영수증)을 저장소가
                // 한 커밋으로 한다(승인 대기 중이라 현재 claim이 아니라 카드와 인증된 소유자가 펜스다). 이 클릭은 낡은 버튼이다.
                Ok(Check::Unrouted(target)) => {
                    self.replies
                        .finish_unsent(
                            DeliveryFence::Approval {
                                nonce: nonce.clone(),
                                actor: actor.clone(),
                            },
                            target,
                            UnsentReason::Unrouted,
                        )
                        .await?;
                    return Err(Error::Stale);
                }
                Err(error) => return Err(error),
            }
        }
        ledger
            .decide(Decision {
                nonce,
                actor,
                allow,
            })
            .await
            .map(|_| ())
    }

    /// 승인 화면이 보여 준 조건(위험 등급·이유·정책·도구 판본·승인자·수신자)이 지금도 같은가.
    async fn conditions_unchanged(
        &self,
        stored: &StoredApproval,
        ctx: &bot_core::state::RunContext,
    ) -> Result<Check, Error> {
        let approval = &stored.approval;
        let (snapshot, tool_revision, class, approver) = if approval.tool.0 == SHARE_TOOL {
            // 내장 기억 공유: 카탈로그에 없다. 분류는 고정이고 스코프·소유자 채널이 지금도 맞아야 한다.
            let catalog = &self.parts.catalog;
            let profile = &approval.attempt.profile;
            let current = catalog.current(profile).ok_or(Error::Denied)?;
            let pinned = catalog
                .lookup(profile, &ctx.work.revision)
                .ok_or(Error::Missing)?;
            authorize(&share_tool().scopes, ctx, &current, &pinned).map_err(|_| Error::Denied)?;
            let approver = share_approver(ctx, &current);
            (
                current,
                Revision(SHARE_REVISION.into()),
                share_class(),
                approver,
            )
        } else if approval.tool.0 == DELIVER_TOOL {
            // 내장 예약 회신 전송: 카탈로그에 없다. 예약을 만들 때 필요했던 스코프와 사람 요청자(= 승인자)가 지금도 맞아야 하고 분류는 고정이다.
            // 초안이 그대로인지는 저장소가 승인을 소비하는 커밋에서 다시 대조한다.
            let catalog = &self.parts.catalog;
            let profile = &approval.attempt.profile;
            let current = catalog.current(profile).ok_or(Error::Denied)?;
            let pinned = catalog
                .lookup(profile, &ctx.work.revision)
                .ok_or(Error::Missing)?;
            authorize(&deliver_tool().scopes, ctx, &current, &pinned).map_err(|_| Error::Denied)?;
            if !ctx.principal.is_human() {
                return Ok(Check::Changed);
            }
            // 카드에 보인 전송 초안의 수신자가 지금 등록부에서도 같은 경로여야 한다(빠졌거나 재지정됐으면 승인하지 않는다).
            let target = delivery_destination(&stored.card, approval.args_sha256)?;
            match self.route_state(profile, &target) {
                Route::Same => {}
                Route::Gone => return Ok(Check::Unrouted(target)),
                // 확인하지 못했다: 버튼을 무효로 하지 않고 이 클릭만 다시 시도하게 한다
                Route::Unsure => return Err(Error::Unavailable),
            }
            let class = deliver_class(&current, ctx);
            let approver = deliver_approver(ctx, &current);
            (current, Revision(DELIVER_REVISION.into()), class, approver)
        } else if let Some(spec) = schedule::mutating_tool(&approval.tool.0) {
            // 내장 예약 생성·취소: 카탈로그에 없다. 같은 이름의 스코프와 사람 요청자가 지금도 맞아야 하고 분류는 정책이 정한다.
            let catalog = &self.parts.catalog;
            let profile = &approval.attempt.profile;
            let current = catalog.current(profile).ok_or(Error::Denied)?;
            let pinned = catalog
                .lookup(profile, &ctx.work.revision)
                .ok_or(Error::Missing)?;
            authorize(&spec.scopes, ctx, &current, &pinned).map_err(|_| Error::Denied)?;
            if !ctx.principal.is_human() {
                return Ok(Check::Changed);
            }
            // 다른 수신자를 지정한 생성: 카드에 보인 정규 인자의 수신자가 지금 등록부에서도 같은 경로여야 하고, 분류는 항상 irreversible이다.
            let destination = if approval.tool.0 == CREATE_TOOL {
                created_destination(&stored.card, approval.args_sha256)?
            } else {
                None
            };
            let spec = match &destination {
                Some(target) => match self.route_state(profile, target) {
                    Route::Same => create_tool_to(),
                    // 빠졌거나 재지정됐다: 옛 버튼은 무효다(만들 것이 없다). 확인하지 못했으면 이 클릭만 다시 시도하게 한다
                    Route::Gone => return Ok(Check::Changed),
                    Route::Unsure => return Err(Error::Unavailable),
                },
                None => spec,
            };
            let class = classify(&spec, &current.policy.approve, ctx.private_memory_read);
            let approver = approver_for(ctx, &current);
            (current, Revision(SCHEDULE_REVISION.into()), class, approver)
        } else if let Some(spec) = bot_core::builtin::manage_tool(&approval.tool.0) {
            let catalog = &self.parts.catalog;
            let profile = &approval.attempt.profile;
            let current = catalog.current(profile).ok_or(Error::Denied)?;
            let pinned = catalog
                .lookup(profile, &ctx.work.revision)
                .ok_or(Error::Missing)?;
            if !current.is_super_admin(&ctx.principal) || !pinned.is_super_admin(&ctx.principal) {
                return Err(Error::Denied);
            }
            let decl = spec.declaration();
            authorize(&decl.scopes, ctx, &current, &pinned).map_err(|_| Error::Denied)?;
            let class = classify(&decl, &current.policy.approve, ctx.private_memory_read);
            let approver = match openai::for_run(ctx, &pinned, &current) {
                Some(Ok(found)) => {
                    receipt_matches(stored, &found)?;
                    match self.card_site(stored, &current, &found) {
                        Route::Same => {}
                        Route::Gone => return Ok(Check::Changed),
                        Route::Unsure => return Err(Error::Unavailable),
                    }
                    Some(found.route.person().clone())
                }
                Some(Err(_)) => None,
                None => approver_for(ctx, &current),
            };
            (
                current,
                Revision(bot_core::builtin::REVISION.into()),
                class,
                approver,
            )
        } else {
            let Decl {
                resolved,
                current: snapshot,
                pinned,
                ..
            } = self.declarations(ctx, &approval.attempt.profile, &approval.tool)?;
            let class = classify(
                &resolved.tool,
                &snapshot.policy.approve,
                ctx.private_memory_read,
            );
            // 지금 판본이 지정한 승인자. 카드가 보여 준 승인자와 같아야 한다(바뀌었으면 옛 승인자의 버튼은 거절한다).
            // OpenAI 출처 실행은 고정·현재 판본이 같은 사람·경로를 가리킬 때만이고, Slack 경로는 확정한 게시 영수증이 그 사람·전달 바인딩의 것이어야 누를 수 있다.
            let approver = match openai::for_run(ctx, &pinned, &snapshot) {
                Some(Ok(found)) => {
                    receipt_matches(stored, &found)?;
                    // 카드가 올라간 설치가 지금 호스트의 같은 설치여야 한다(같은 별칭의 team_id만 바뀐 재배포의 옛 카드). 어긋나면 조건이 바뀐 것과 같다: 옛 버튼은 무효이고
                    // 깨어난 run이 새 카드를 열면 원래 요청에 고정한 설치와 달라 게시 전에 `route_changed`로 닫힌다. 확인하지 못했으면 이 클릭만 다시 시도하게 한다.
                    match self.card_site(stored, &snapshot, &found) {
                        Route::Same => {}
                        Route::Gone => return Ok(Check::Changed),
                        Route::Unsure => return Err(Error::Unavailable),
                    }
                    Some(found.route.person().clone())
                }
                Some(Err(_)) => None,
                None => approver_for(ctx, &snapshot),
            };
            (snapshot, resolved.tool.revision.clone(), class, approver)
        };
        let Some(approver) = approver else {
            return Ok(Check::Changed);
        };
        let approver = &approver;
        let expected = Binding {
            attempt: &approval.attempt,
            call: &approval.call,
            requester: &ctx.principal,
            approver,
            class: &class,
            policy_revision: &snapshot.policy_revision,
            tool: &approval.tool,
            tool_revision: &tool_revision,
            args_sha256: approval.args_sha256,
        }
        .approval();
        Ok(
            if same_binding(approval, &expected) && stored.reasons == class.codes() {
                Check::Unchanged
            } else {
                Check::Changed
            },
        )
    }
}
