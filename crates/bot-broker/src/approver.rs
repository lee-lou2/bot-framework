//! Resolves the active human approver for a run from the current profile snapshot.
//!
//! Ignores `RunContext.approver` recorded at intake so revision changes never route cards to stale approvers:
//! - Scheduled runs (including delegated children): requires the exact `RunContext.trigger` cron entry on the
//!   current snapshot with the same service principal and an explicit approver.
//! - Other runs: the requester themselves.
//!
//! Returns `None` unless the candidate is a human member of the current snapshot.

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
        ctx.approver = Some(PrincipalId("local:owner".into()));
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
            "no approver configured"
        );
        assert!(
            approver_for(&ctx, &world(Some("service:morning"))).is_none(),
            "service principal cannot approve"
        );
        assert!(
            approver_for(&ctx, &world(Some("local:ghost"))).is_none(),
            "not a profile member"
        );
        let gone = snapshot("personal", "r2")
            .role("owner", &["a"])
            .member("local:owner", &["owner"])
            .policy_scopes(&["a"])
            .build();
        assert!(approver_for(&ctx, &gone).is_none(), "trigger removed");
        let other = scheduled("service:other");
        assert!(
            approver_for(&other, &world(Some("local:alice"))).is_none(),
            "trigger belongs to a different service principal"
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
        // Must not fall back to another trigger sharing the same principal.
        assert!(approver_for(&ctx, &build(None)).is_none());
    }

    #[test]
    fn a_scheduled_run_without_a_surviving_origin_cannot_be_confirmed() {
        let snap = world(Some("local:alice"));
        let mut lost = scheduled("service:morning");
        lost.trigger = None;
        assert!(approver_for(&lost, &snap).is_none(), "missing origin");
        let mut renamed = scheduled("service:morning");
        renamed.trigger = Some(TriggerRef {
            agent: AgentId("main".into()),
            id: "gone".into(),
        });
        assert!(approver_for(&renamed, &snap).is_none(), "trigger deleted");
        let mut elsewhere = scheduled("service:morning");
        elsewhere.trigger = Some(TriggerRef {
            agent: AgentId("other".into()),
            id: "morning".into(),
        });
        assert!(
            approver_for(&elsewhere, &snap).is_none(),
            "trigger belongs to another agent"
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
        child.agent = AgentId("main/research".into());
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
            "http principal cannot approve"
        );
    }
}
