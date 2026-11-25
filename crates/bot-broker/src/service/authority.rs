//! 직접 호출의 권한(F1 사전 승인·F4 지정한 사람의 증거) 판정. 판정은 브로커가 하고, 저장소가 호출을 시작하는 커밋에서 같은 잠금으로 다시 대조한다.
//! 권한은 루트 run의 직접 호출(`{run}:tool`)에만 붙는다: 모델·위임한 자식·다른 후보·본문 속 트리거 ID는 어느 쪽도 물려받지 못한다.

use super::{BrokerService, call::Flow};
use crate::{
    classify::{Class, Reason},
    preapproval,
};
use bot_core::{
    command::Target,
    grant::{Authority, reason},
    profile::{Snapshot, Trigger, TriggerSource, TriggerTarget},
    state::ConversationKind,
};

/// 이 호출에 붙는 권한 판정.
pub(super) enum Verdict {
    /// 해당 선언이 없다: 기존 규칙(카드·승인자)을 그대로 따른다.
    None,
    /// 이 권한으로 시작을 시도한다.
    Use(Authority),
    /// 선언은 있지만 지금은 쓸 수 없다: 기존 승인 경로로 대신하지 않고 이 사유로 닫는다.
    Deny(&'static str),
}

/// 위험 이유가 승인 한 단계(`Irreversible`·`Policy`)뿐인가. 민감 회상 뒤 확인 같은 독립적인 요건은 대신하지 않는다.
pub(super) fn waivable(class: &Class) -> bool {
    class
        .reasons
        .iter()
        .all(|r| matches!(r, Reason::Irreversible | Reason::Policy))
}

/// `snapshot`에서 에이전트가 소유한 트리거 하나.
fn owned<'a>(
    snapshot: &'a Snapshot,
    agent: &bot_core::id::AgentId,
    id: &str,
) -> Option<&'a Trigger> {
    snapshot
        .agents
        .get(agent)?
        .triggers
        .iter()
        .find(|t| t.id == id)
}

impl BrokerService {
    pub(super) async fn authority_for(&self, flow: &Flow, class: &Class, pointer: bool) -> Verdict {
        let ctx = &flow.ctx;
        let direct = ctx.root == flow.attempt.run
            && flow.call.0 == format!("{}:tool", flow.attempt.run.0)
            && matches!(&ctx.work.target, Target::Tool { tool, .. } if *tool == flow.tool);
        if !direct {
            return Verdict::None;
        }
        match ctx.conversation.kind {
            ConversationKind::Scheduled => self.grant_authority(flow),
            _ => self.human_authority(flow, class, pointer).await,
        }
    }

    /// F1: 지금 판본이 이 트리거에 `preapproved`를 선언했으면, 지금 선언·설치 판본으로 지문을 다시 계산해 권한으로 낸다.
    fn grant_authority(&self, flow: &Flow) -> Verdict {
        let ctx = &flow.ctx;
        let Some(origin) = ctx.trigger.as_ref().filter(|o| o.agent == ctx.agent) else {
            return Verdict::None;
        };
        let Some(trigger) = owned(&flow.snapshot, &ctx.agent, &origin.id) else {
            return Verdict::None;
        };
        if trigger.preapproved.is_none() {
            return Verdict::None;
        }
        let same_principal = matches!(
            &trigger.on,
            TriggerSource::Cron { principal, .. } if *principal == ctx.principal
        );
        let image = self.parts.runner.installed(&flow.resolved.tool.server);
        let spec = image.filter(|_| same_principal).and_then(|image| {
            preapproval::grant_spec(
                &flow.snapshot,
                &ctx.agent,
                trigger,
                &ctx.scopes,
                &image,
                &flow.env,
            )
        });
        match spec {
            Some(spec) => Verdict::Use(Authority::Grant {
                fingerprint: spec.fingerprint,
            }),
            None => Verdict::Deny(reason::STALE),
        }
    }

    /// F4: 승인이 필요한 호출이고 접수 때 묶은 증거가 지금·고정 판본이 지정한 승인자의 이번 이벤트일 때만 권한으로 낸다.
    /// 어긋나면 증거는 쓰지 않고(감사에 고정 사유만 남긴다) 기존 카드 규칙을 따른다.
    async fn human_authority(&self, flow: &Flow, class: &Class, pointer: bool) -> Verdict {
        if !class.needs_approval() || pointer {
            return Verdict::None;
        }
        let Ok(Some(evidence)) = self.parts.ledger.evidence(flow.attempt.clone()).await else {
            return Verdict::None;
        };
        let ctx = &flow.ctx;
        let designated = |snapshot: &Snapshot| {
            let Some(trigger) = owned(snapshot, &ctx.agent, &evidence.trigger) else {
                return false;
            };
            matches!(&trigger.on, TriggerSource::Event { binding, .. } if *binding == evidence.binding)
                && !trigger.private
                && matches!(&trigger.target, TriggerTarget::Tool { tool, .. } if *tool == flow.tool)
                && trigger
                    .human_approval
                    .as_ref()
                    .is_some_and(|h| h.approvers.contains(&evidence.actor))
                && evidence.actor.is_human()
                && snapshot.members.contains_key(&evidence.actor)
        };
        let fits = waivable(class)
            && ctx.principal == evidence.actor
            && ctx.binding == evidence.binding
            && designated(&flow.snapshot)
            && designated(&flow.pinned);
        if fits {
            return Verdict::Use(Authority::Human {
                actor: evidence.actor,
            });
        }
        let _ = self
            .note(
                &flow.attempt.profile,
                &ctx.principal,
                "call.note",
                &flow.tool,
                Some(flow.sealed.sha256),
                reason::HUMAN_MISMATCH,
            )
            .await;
        Verdict::None
    }
}
