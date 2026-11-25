//! 승인자 확인: 지금 활성인 판본이 이 실행의 승인자로 지정한 사람.
//!
//! 실행이 접수 때 기록한 승인자(`RunContext.approver`)는 쓰지 않는다. 판본이 바뀌면 승인 카드가 옛 승인자에게 가면 안 된다.
//! - 예약 실행(자식 포함): 실행이 보존한 루트 출처(`RunContext.trigger` = 에이전트 + 트리거 ID)와 **정확히 같은** 트리거가
//!   지금 판본에 cron으로 있고, 같은 서비스 주체를 쓰고, 승인자가 지정돼 있어야 한다. 출처가 없거나 사라졌으면 다른
//!   트리거로 대신하지 않고 확인할 수 없다.
//! - 그 밖의 실행: 요청자 본인.
//!
//! 어느 쪽이든 사람 신원이고 지금 판본의 멤버여야 한다. 확인할 수 없으면 `None`이고 승인이 필요한 호출은 거절한다.

pub mod openai;

use bot_core::{
    id::PrincipalId,
    profile::{Snapshot, TriggerSource},
    state::{ConversationKind, RunContext},
};

pub fn approver_for(ctx: &RunContext, snapshot: &Snapshot) -> Option<PrincipalId> {
    let candidate = match ctx.conversation.kind {
        ConversationKind::Scheduled => {
            let origin = ctx.trigger.as_ref()?;
            let owner = snapshot.agents.get(&origin.agent)?;
            let trigger = owner.triggers.iter().find(|t| t.id == origin.id)?;
            match &trigger.on {
                TriggerSource::Cron {
                    principal,
                    approver: Some(approver),
                    ..
                } if *principal == ctx.principal => approver.clone(),
                _ => return None,
            }
        }
        _ => ctx.principal.clone(),
    };
    (candidate.is_human() && snapshot.members.contains_key(&candidate)).then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{run_context, snapshot};
    use bot_core::{id::AgentId, state::TriggerRef};
    use std::sync::Arc;

    fn world(approver: Option<&str>) -> Arc<Snapshot> {
        snapshot("personal", "r1")
            .role("owner", &["a"])
            .member("local:owner", &["owner"])
            .member("local:alice", &["owner"])
            .member("service:morning", &["owner"])
            .policy_scopes(&["a"])
            .cron_trigger("main", "morning", "service:morning", approver)
            .build()
    }

    fn scheduled(principal: &str) -> RunContext {
        let mut ctx = run_context("personal", "r1", 1, principal, "main", &["a"]);
        ctx.conversation.kind = ConversationKind::Scheduled;
        ctx.trigger = Some(TriggerRef {
            agent: AgentId("main".into()),
            id: "morning".into(),
        });
        ctx
    }

    #[test]
    fn a_person_who_asked_approves_for_themselves() {
        let ctx = run_context("personal", "r1", 1, "local:owner", "main", &["a"]);
        assert_eq!(approver_for(&ctx, &world(None)).unwrap().0, "local:owner");
    }

    #[test]
    fn a_scheduled_run_follows_the_current_trigger_not_the_recorded_approver() {
        let mut ctx = scheduled("service:morning");
        ctx.approver = Some(PrincipalId("local:owner".into())); // 접수 때 기록한 옛 승인자
        assert_eq!(
            approver_for(&ctx, &world(Some("local:alice"))).unwrap().0,
            "local:alice"
        );
    }

    #[test]
    fn nobody_is_asked_when_the_approver_cannot_be_confirmed() {
        let ctx = scheduled("service:morning");
        assert!(
            approver_for(&ctx, &world(None)).is_none(),
            "승인자를 지정하지 않았다"
        );
        assert!(
            approver_for(&ctx, &world(Some("service:morning"))).is_none(),
            "서비스 주체는 승인할 수 없다"
        );
        assert!(
            approver_for(&ctx, &world(Some("local:ghost"))).is_none(),
            "멤버가 아니다"
        );
        let gone = snapshot("personal", "r2")
            .role("owner", &["a"])
            .member("local:owner", &["owner"])
            .policy_scopes(&["a"])
            .build();
        assert!(approver_for(&ctx, &gone).is_none(), "트리거가 사라졌다");
        let other = scheduled("service:other");
        assert!(
            approver_for(&other, &world(Some("local:alice"))).is_none(),
            "다른 서비스 주체의 트리거가 아니다"
        );
    }

    #[test]
    fn only_the_exact_origin_trigger_counts_even_when_another_uses_the_same_principal() {
        let build = |morning: Option<&str>| {
            snapshot("personal", "r1")
                .member("local:owner", &[])
                .member("local:alice", &[])
                .cron_trigger("main", "morning", "service:morning", morning)
                .cron_trigger("main", "evening", "service:morning", Some("local:alice"))
                .build()
        };
        let ctx = scheduled("service:morning");
        assert_eq!(
            approver_for(&ctx, &build(Some("local:owner"))).unwrap().0,
            "local:owner"
        );
        // 원래 트리거의 승인자가 지워져도 같은 주체의 다른 트리거(evening)의 승인자에게 가지 않는다
        assert!(approver_for(&ctx, &build(None)).is_none());
    }

    #[test]
    fn a_scheduled_run_without_a_surviving_origin_cannot_be_confirmed() {
        let snap = world(Some("local:alice"));
        let mut lost = scheduled("service:morning");
        lost.trigger = None;
        assert!(approver_for(&lost, &snap).is_none(), "출처가 없다");
        let mut renamed = scheduled("service:morning");
        renamed.trigger = Some(TriggerRef {
            agent: AgentId("main".into()),
            id: "gone".into(),
        });
        assert!(approver_for(&renamed, &snap).is_none(), "트리거가 삭제됐다");
        let mut elsewhere = scheduled("service:morning");
        elsewhere.trigger = Some(TriggerRef {
            agent: AgentId("other".into()),
            id: "morning".into(),
        });
        assert!(
            approver_for(&elsewhere, &snap).is_none(),
            "다른 에이전트의 트리거다"
        );
    }

    #[test]
    fn a_delegated_child_keeps_the_root_origin() {
        let snap = snapshot("personal", "r1")
            .member("local:alice", &[])
            .agent("main/research")
            .cron_trigger("main", "morning", "service:morning", Some("local:alice"))
            .build();
        let mut child = scheduled("service:morning");
        child.agent = AgentId("main/research".into()); // 자식이 일하지만 출처는 루트의 트리거다
        assert_eq!(approver_for(&child, &snap).unwrap().0, "local:alice");
    }

    #[test]
    fn a_requester_who_left_the_profile_has_no_approver() {
        let ctx = run_context("personal", "r1", 1, "local:owner", "main", &["a"]);
        let gone = snapshot("personal", "r2").build();
        assert!(approver_for(&ctx, &gone).is_none());
        let mut http = run_context("personal", "r1", 1, "http:inbox/u1", "main", &["a"]);
        http.conversation.kind = ConversationKind::Http;
        let snap = snapshot("personal", "r3")
            .member("http:inbox/u1", &[])
            .build();
        assert!(
            approver_for(&http, &snap).is_none(),
            "http 신원은 승인하지 못한다"
        );
    }
}
